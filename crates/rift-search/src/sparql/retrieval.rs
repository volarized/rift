//! Selects existing retrieval producers from explicit local SERVICE patterns.

use std::collections::BTreeSet;

use oxrdf::{
    NamedNode,
    vocab::{rdf, xsd},
};
use rift_error::{RiftError, errors};
use rift_protocol::read::SearchParams;
use serde_json::{Map, Value};
use spargebra::algebra::QueryExpression;
use spargebra::term::{NamedNodePattern, TermPattern};

use super::VOCABULARY_NAMESPACE;

const SEARCH_SERVICE: &str = "urn:rift:vocabulary:search";
const SEARCH_GRAPH_PREFIX: &str = "urn:rift:retrieval:";
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
    if *silent
        || !matches!(name, NamedNodePattern::NamedNode(name) if name.as_str() == SEARCH_SERVICE)
    {
        return super::query::refused(&format!("SERVICE {name}"));
    }
    let mut fields = Map::new();
    let mut include = BTreeSet::new();
    let mut pending = vec![(inner.as_mut(), true)];
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
                    if !INPUT_FIELDS.contains(&key)
                        || matches!(&triple.object, TermPattern::Variable(_))
                    {
                        continue;
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
                    triple.object = TermPattern::NamedNode(NamedNode::new_unchecked(format!(
                        "{VOCABULARY_NAMESPACE}SearchHit"
                    )));
                }
            }
            QueryExpression::Join { left, right } => {
                pending.extend([(left.as_mut(), required), (right.as_mut(), required)])
            }
            QueryExpression::LeftJoin { left, right, .. }
            | QueryExpression::Minus { left, right } => {
                pending.extend([(left.as_mut(), required), (right.as_mut(), false)]);
            }
            QueryExpression::Union { left, right } => {
                pending.extend([(left.as_mut(), false), (right.as_mut(), false)])
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
            Value::Array(
                include
                    .into_iter()
                    .map(|field| Value::String(field.to_owned()))
                    .collect(),
            ),
        );
    }
    let request = serde_json::from_value(Value::Object(fields))
        .map_err(|error| invalid(&error.to_string()))?;
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
    Ok(RetrievalSelection { graph, request })
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
        _ => Err(invalid("retrieval inputs must be constant")),
    }
}

fn invalid(detail: &str) -> RiftError {
    errors::search::query_syntax_invalid()
        .detail(detail)
        .error()
}
