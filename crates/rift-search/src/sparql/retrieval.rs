//! Selects existing retrieval producers from explicit local SERVICE patterns.

use std::collections::BTreeSet;

use oxrdf::{
    NamedNode,
    vocab::{rdf, xsd},
};
use rift_error::{RiftError, errors};
use rift_protocol::read::{SearchInclude, SearchParams, SearchParamsTarget, SearchScope};
use serde_json::{Map, Value};
use spargebra::algebra::QueryExpression;
use spargebra::term::{NamedNodePattern, TermPattern};

use super::VOCABULARY_NAMESPACE;
use crate::flow::FLOW_RANKED_ROWS_MAX;

const SEARCH_SERVICE: &str = "urn:rift:vocabulary:search";
const FLOWS_SERVICE: &str = "urn:rift:vocabulary:flows";
pub(super) const FLOW_CLASS: &str = "Flow";
const SEARCH_GRAPH_PREFIX: &str = "urn:rift:retrieval:";
const FLOW_LIMIT_DEFAULT: u64 = 5;
const FLOW_LIMIT_MAX: u64 = 32;
pub(super) const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const INPUT_FIELDS: &[&str] = &[
    "view",
    "query",
    "pattern",
    "scope",
    "target",
    "order",
    "packages",
    "paths",
    "include",
    "limit",
    "page_index",
    "rev",
    "change",
];

/// One statically selected invocation of an existing retrieval producer.
#[derive(Clone, Debug)]
pub struct RetrievalSelection {
    pub(super) graph: NamedNode,
    /// Producer parameters selected by the service's constant input triples.
    pub request: SearchParams,
    /// Maximum complete flow records, when the service selects call paths.
    pub flow_limit: Option<usize>,
}

pub(super) fn rewrite(
    expression: &mut QueryExpression,
    index: usize,
) -> Result<RetrievalSelection, RiftError> {
    let QueryExpression::Service {
        name,
        inner,
        silent,
    } = expression
    else {
        return Err(invalid("expected a retrieval service"));
    };
    let service = match name {
        NamedNodePattern::NamedNode(name) if !*silent => name.as_str(),
        _ => "",
    };
    if ![SEARCH_SERVICE, FLOWS_SERVICE].contains(&service) {
        return super::query::refused(&format!("SERVICE {name}"));
    }
    let flows = service == FLOWS_SERVICE;
    let mut request = request(inner, flows)?;
    let flow_limit = flows
        .then(|| prepare_flow_request(&mut request))
        .transpose()?;
    let graph = NamedNode::new_unchecked(format!("{SEARCH_GRAPH_PREFIX}{index}"));
    let inner = std::mem::replace(
        inner,
        Box::new(QueryExpression::Bgp {
            patterns: Vec::new(),
        }),
    );
    *expression = QueryExpression::Graph {
        name: NamedNodePattern::NamedNode(graph.clone()),
        inner,
    };
    Ok(RetrievalSelection {
        graph,
        request,
        flow_limit,
    })
}

fn prepare_flow_request(request: &mut SearchParams) -> Result<usize, RiftError> {
    if request
        .query
        .as_ref()
        .is_none_or(|query| query.trim().is_empty())
    {
        return Err(invalid("flow search requires a nonempty constant query"));
    }
    let limit = request.limit.unwrap_or(FLOW_LIMIT_DEFAULT);
    if !(1..=FLOW_LIMIT_MAX).contains(&limit) {
        return Err(invalid("flow limit must be between 1 and 32"));
    }
    request.scope = SearchScope::Local;
    request.target = SearchParamsTarget::Symbol;
    request.limit = Some(FLOW_RANKED_ROWS_MAX);
    request.include = Some(vec![SearchInclude::Score]);
    usize::try_from(limit).map_err(|error| invalid(&error.to_string()))
}

fn request(inner: &mut QueryExpression, flows: bool) -> Result<SearchParams, RiftError> {
    let mut fields = Map::new();
    let mut include = BTreeSet::new();
    let mut pending = vec![(inner, true)];
    while let Some((pattern, required)) = pending.pop() {
        match pattern {
            QueryExpression::Bgp { patterns } => {
                for triple in patterns {
                    let NamedNodePattern::NamedNode(predicate) = &triple.predicate else {
                        continue;
                    };
                    let Some(key) = predicate.as_str().strip_prefix(VOCABULARY_NAMESPACE) else {
                        continue;
                    };
                    if matches!(key, "source" | "score") {
                        include.insert(key.to_owned());
                    }
                    if flows
                        && INPUT_FIELDS.contains(&key)
                        && matches!(&triple.object, TermPattern::Variable(_))
                    {
                        return Err(invalid("flow search inputs must be constant"));
                    }
                    if !INPUT_FIELDS.contains(&key)
                        || matches!(&triple.object, TermPattern::Variable(_))
                    {
                        continue;
                    }
                    if flows && !matches!(key, "query" | "limit") {
                        return Err(invalid(
                            "flow search accepts constant query and limit inputs",
                        ));
                    }
                    if flows
                        && key == "limit"
                        && !matches!(&triple.object, TermPattern::Literal(value) if *value.datatype() == xsd::INTEGER)
                    {
                        return Err(invalid("flow limit must be an integer literal"));
                    }
                    if !required {
                        return Err(invalid(
                            "retrieval inputs must be required constant triples",
                        ));
                    }
                    let value = input(key, &triple.object)?;
                    let field = if key == "rev" { "revision" } else { key };
                    if fields
                        .insert(field.to_owned(), value.clone())
                        .is_some_and(|previous| previous != value)
                    {
                        return Err(invalid("retrieval input carries conflicting values"));
                    }
                    triple.predicate = NamedNodePattern::NamedNode(rdf::TYPE);
                    let class = if flows { FLOW_CLASS } else { "SearchHit" };
                    triple.object = TermPattern::NamedNode(NamedNode::new_unchecked(format!(
                        "{VOCABULARY_NAMESPACE}{class}"
                    )));
                }
            }
            QueryExpression::Join { left, right } => {
                pending.extend([(left.as_mut(), required), (right.as_mut(), required)]);
            }
            QueryExpression::LeftJoin { left, right, .. }
            | QueryExpression::Minus { left, right } => {
                pending.extend([(left.as_mut(), required), (right.as_mut(), false)]);
            }
            QueryExpression::Union { left, right } => {
                pending.extend([(left.as_mut(), false), (right.as_mut(), false)]);
            }
            QueryExpression::Filter { inner, .. }
            | QueryExpression::Extend { inner, .. }
            | QueryExpression::OrderBy { inner, .. }
            | QueryExpression::Project { inner, .. }
            | QueryExpression::Distinct { inner, .. }
            | QueryExpression::Reduced { inner, .. }
            | QueryExpression::Slice { inner, .. }
            | QueryExpression::Group { inner, .. } => pending.push((inner, required)),
            QueryExpression::Values { .. } | QueryExpression::Path { .. } => {}
            _ => {
                return Err(invalid(
                    "retrieval service cannot acquire another graph or service",
                ));
            }
        }
    }
    if !include.is_empty() && !fields.contains_key("include") {
        fields.insert(
            "include".to_owned(),
            Value::Array(include.into_iter().map(Value::String).collect()),
        );
    }
    serde_json::from_value(Value::Object(fields)).map_err(|error| invalid(&error.to_string()))
}

fn input(key: &str, object: &TermPattern) -> Result<Value, RiftError> {
    if matches!(key, "query" | "pattern")
        && !matches!(object, TermPattern::Literal(value) if *value.datatype() == xsd::STRING)
    {
        return Err(invalid(
            "retrieval query and pattern must be string literals",
        ));
    }
    match object {
        TermPattern::Literal(value) if value.datatype().as_str() == RDF_JSON => {
            serde_json::from_str(value.value()).map_err(|error| invalid(&error.to_string()))
        }
        TermPattern::Literal(value) if matches!(key, "limit" | "page_index") => value
            .value()
            .parse::<u64>()
            .map(Value::from)
            .map_err(|error| invalid(&error.to_string())),
        TermPattern::Literal(value) => Ok(Value::String(value.value().to_owned())),
        TermPattern::NamedNode(name) => Ok(Value::String(
            name.as_str()
                .strip_prefix(VOCABULARY_NAMESPACE)
                .unwrap_or(name.as_str())
                .to_owned(),
        )),
        TermPattern::Variable(_) => Err(invalid("retrieval inputs must be constant")),
    }
}

fn invalid(detail: &str) -> RiftError {
    errors::search::query_syntax_invalid()
        .detail(detail)
        .error()
}

#[cfg(test)]
mod tests {
    use crate::sparql::{GraphLimits, GraphQuery};
    use rift_error::errors;
    use rift_protocol::read::{SearchInclude, SearchParamsTarget, SearchScope};

    #[test]
    fn flow_service_selects_ranked_local_symbols_and_limits_whole_records()
    -> Result<(), rift_error::RiftError> {
        let limits = GraphLimits::default();
        let query = GraphQuery::parse(
            "SELECT ?flow { SERVICE :flows { ?flow :query 'middleware'; :limit 2 } }",
            &limits,
        )?;
        let [selection] = query.retrieval_requests() else {
            panic!("expected one flow producer");
        };
        assert_eq!(selection.flow_limit, Some(2));
        assert_eq!(selection.request.scope, SearchScope::Local);
        assert_eq!(selection.request.target, SearchParamsTarget::Symbol);
        assert_eq!(selection.request.query.as_deref(), Some("middleware"));
        assert_eq!(
            selection.request.include.as_deref(),
            Some([SearchInclude::Score].as_slice())
        );
        Ok(())
    }

    #[test]
    fn flow_service_refuses_variable_conflicting_or_unsupported_inputs() {
        let limits = GraphLimits::default();
        for input in [
            "?flow :query ?query",
            "?flow :query 'a', 'b'",
            "?flow :query 'a'; :scope :all",
            "?flow :query 'a'; :limit 0",
            "?flow :query 'a'; :limit 33",
            "?flow :query 'a'; :limit '5'",
            "?flow :query 'a'; :limit ?limit",
            "?flow :query ''",
            "?flow :limit 5",
            "?flow :query 'a' OPTIONAL { ?flow :limit 3 }",
        ] {
            let query = format!("SELECT ?flow {{ SERVICE :flows {{ {input} }} }}");
            let error = GraphQuery::parse(&query, &limits).expect_err(input);
            assert_eq!(
                error.slug(),
                errors::search::query_syntax_invalid::SLUG,
                "{input}"
            );
        }
    }
}
