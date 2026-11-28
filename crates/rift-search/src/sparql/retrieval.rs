//! Selects existing retrieval producers from explicit local SERVICE patterns.

use oxrdf::{
    NamedNode,
    vocab::{rdf, xsd},
};
use rift_error::{RiftError, errors};
use rift_protocol::query::RetrievalInclude;
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
/// The input a service selects its scope with.
const SCOPE_FIELD: &str = "scope";
const INPUT_FIELDS: &[&str] = &[
    "view",
    "query",
    "pattern",
    SCOPE_FIELD,
    "target",
    "order",
    "packages",
    "paths",
    "include",
    "max_results",
    "page",
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
    scope_choice: ScopeChoice,
}

impl RetrievalSelection {
    /// Selects `scope` when the service left the choice to the workspace.
    pub(super) fn select_default_scope(&mut self, scope: &SearchScope) {
        match self.scope_choice {
            ScopeChoice::Workspace => self.request.scope.clone_from(scope),
            ScopeChoice::Service => {}
        }
    }
}

/// Who chooses the scope one service reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScopeChoice {
    /// The service itself: a `SERVICE :search` that selects `:scope`, or `SERVICE :flows`,
    /// which reads `local` alone.
    Service,
    /// The workspace: a `SERVICE :search` that selects no `:scope`.
    Workspace,
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
    let (mut request, scope_choice) = request(inner, flows)?;
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
        scope_choice,
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
    let limit = request.max_results.unwrap_or(FLOW_LIMIT_DEFAULT);
    if !(1..=FLOW_LIMIT_MAX).contains(&limit) {
        return Err(invalid("flow limit must be between 1 and 32"));
    }
    request.scope = SearchScope::Local;
    request.target = SearchParamsTarget::Symbol;
    request.max_results = Some(FLOW_RANKED_ROWS_MAX);
    request.include = SearchInclude {
        score: true,
        ..SearchInclude::default()
    };
    usize::try_from(limit).map_err(|error| invalid(&error.to_string()))
}

fn request(
    inner: &mut QueryExpression,
    flows: bool,
) -> Result<(SearchParams, ScopeChoice), RiftError> {
    let mut fields = Map::new();
    let mut inferred = SearchInclude::default();
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
                    inferred.source |= key == "source";
                    inferred.score |= key == "score";
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
                    if flows && !matches!(key, "query" | "max_results") {
                        return Err(invalid(
                            "flow search accepts constant query and limit inputs",
                        ));
                    }
                    if flows
                        && key == "max_results"
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
    let include = fields
        .remove("include")
        .map(serde_json::from_value::<RetrievalInclude>)
        .transpose()
        .map_err(|error| invalid(&error.to_string()))?;
    let scope_choice = if flows || fields.contains_key(SCOPE_FIELD) {
        ScopeChoice::Service
    } else {
        ScopeChoice::Workspace
    };
    let mut request: SearchParams = serde_json::from_value(Value::Object(fields))
        .map_err(|error| invalid(&error.to_string()))?;
    request.include = include.map_or(inferred, SearchInclude::from);
    refuse_unowned_packages(&request)?;
    Ok((request, scope_choice))
}

/// Refuses a `:packages` entry that names no registry. Such an entry carries no owner, so no
/// release answers for it, and the read would end as an empty answer that names no cause.
fn refuse_unowned_packages(request: &SearchParams) -> Result<(), RiftError> {
    if request
        .packages
        .iter()
        .any(|package| package.registry.is_none())
    {
        return Err(invalid(PACKAGE_REGISTRY_REQUIRED));
    }
    Ok(())
}

/// The refusal of a `:packages` entry that names no `registry`.
const PACKAGE_REGISTRY_REQUIRED: &str = "every `:packages` entry names its `registry`, such as `{\"manager\":\"cargo\",\"registry\":\"crates.io\",\"name\":\"serde\"}`";

/// Inputs that carry a structured value, written as an `rdf:JSON` literal, each with the
/// value its refusal shows.
const JSON_FIELDS: [(&str, &str); 4] = [
    ("include", r#"["source"]"#),
    ("paths", r#"{"include":["src/**"]}"#),
    (
        "packages",
        r#"[{"manager":"cargo","registry":"crates.io","name":"serde"}]"#,
    ),
    ("change", r#"{"base":"main"}"#),
];

fn input(key: &str, object: &TermPattern) -> Result<Value, RiftError> {
    if matches!(key, "query" | "pattern")
        && !matches!(object, TermPattern::Literal(value) if *value.datatype() == xsd::STRING)
    {
        return Err(invalid(
            "retrieval query and pattern must be string literals",
        ));
    }
    if let Some((_, example)) = JSON_FIELDS.iter().find(|(field, _)| *field == key)
        && !matches!(object, TermPattern::Literal(value) if value.datatype().as_str() == RDF_JSON)
    {
        return Err(invalid(&format!(
            "`:{key}` takes an `rdf:JSON` literal, such as `'{example}'^^rdf:JSON`"
        )));
    }
    match object {
        TermPattern::Literal(value) if value.datatype().as_str() == RDF_JSON => {
            serde_json::from_str(value.value()).map_err(|error| invalid(&error.to_string()))
        }
        TermPattern::Literal(value) if matches!(key, "max_results" | "page") => value
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
            "SELECT ?flow { SERVICE :flows { ?flow :query 'middleware'; :max_results 2 } }",
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
            selection.request.include,
            SearchInclude {
                score: true,
                ..SearchInclude::default()
            }
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
            "?flow :query 'a'; :max_results 0",
            "?flow :query 'a'; :max_results 33",
            "?flow :query 'a'; :max_results '5'",
            "?flow :query 'a'; :max_results ?limit",
            "?flow :query ''",
            "?flow :max_results 5",
            "?flow :query 'a' OPTIONAL { ?flow :max_results 3 }",
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

    /// Each selection's constant `:query` paired with the scope it reads, sorted by that query.
    fn scopes_by_query(query: &GraphQuery) -> Vec<(String, SearchScope)> {
        let mut scopes: Vec<_> = query
            .retrieval_requests()
            .iter()
            .map(|selection| {
                let query = selection.request.query.clone().unwrap_or_default();
                (query, selection.request.scope.clone())
            })
            .collect();
        scopes.sort();
        scopes
    }

    fn parsed(text: &str) -> Result<GraphQuery, rift_error::RiftError> {
        GraphQuery::parse(text, &GraphLimits::default())
    }

    #[test]
    fn explicit_retrieval_sections_override_inferred_source_and_score()
    -> Result<(), rift_error::RiftError> {
        let projection = |include| {
            format!(
                "SELECT ?source ?score {{ SERVICE :search {{ ?hit :query 'beacon'; :source ?source; :score ?score {include} }} }}"
            )
        };
        for (include, expected) in [
            (
                "",
                SearchInclude {
                    source: true,
                    score: true,
                    providers: false,
                },
            ),
            ("; :include '[]'^^rdf:JSON", SearchInclude::default()),
            (
                r#"; :include '["providers","source"]'^^rdf:JSON"#,
                SearchInclude {
                    source: true,
                    score: false,
                    providers: true,
                },
            ),
        ] {
            let query = parsed(&projection(include))?;
            assert_eq!(query.retrieval_requests()[0].request.include, expected);
        }
        for include in [
            r#"; :include '{"source":true}'^^rdf:JSON"#,
            r#"; :include '["source","source"]'^^rdf:JSON"#,
            r#"; :include '["other"]'^^rdf:JSON"#,
        ] {
            let error = parsed(&projection(include)).expect_err(include);
            assert_eq!(error.slug(), errors::search::query_syntax_invalid::SLUG);
        }
        Ok(())
    }

    #[test]
    fn test_default_scope_reaches_a_search_service_naming_no_scope()
    -> Result<(), rift_error::RiftError> {
        let text = "SELECT ?hit { SERVICE :search { ?hit :query 'beacon' } }";
        for default_scope in [SearchScope::Local, SearchScope::Global, SearchScope::All] {
            let query = parsed(text)?.with_default_scope(&default_scope);
            assert_eq!(
                scopes_by_query(&query),
                [("beacon".to_owned(), default_scope)]
            );
        }
        Ok(())
    }

    #[test]
    fn test_default_scope_leaves_a_search_service_naming_its_scope()
    -> Result<(), rift_error::RiftError> {
        let named = [
            ("local", SearchScope::Local),
            ("all", SearchScope::All),
            ("global", SearchScope::Global),
            (
                "local@docs",
                SearchScope::NamedLocal {
                    name: "docs".to_owned(),
                },
            ),
        ];
        for (spelling, scope) in named {
            let text = format!(
                "SELECT ?hit {{ SERVICE :search {{ ?hit :query 'beacon'; :scope '{spelling}' }} }}"
            );
            for default_scope in [SearchScope::Global, SearchScope::All] {
                let query = parsed(&text)?.with_default_scope(&default_scope);
                assert_eq!(
                    scopes_by_query(&query),
                    [("beacon".to_owned(), scope.clone())],
                    "{spelling} under {default_scope:?}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn test_default_scope_reaches_only_the_services_naming_no_scope()
    -> Result<(), rift_error::RiftError> {
        let query = parsed(
            "SELECT ?symbol { \
             SERVICE :search { ?first :query 'named'; :scope 'local'; :symbol ?symbol } \
             SERVICE :search { ?second :query 'unnamed'; :symbol ?symbol } }",
        )?
        .with_default_scope(&SearchScope::All);
        assert_eq!(
            scopes_by_query(&query),
            [
                ("named".to_owned(), SearchScope::Local),
                ("unnamed".to_owned(), SearchScope::All),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_default_scope_leaves_a_flow_service_local() -> Result<(), rift_error::RiftError> {
        let text = "SELECT ?flow { SERVICE :flows { ?flow :query 'middleware'; :max_results 2 } }";
        for default_scope in [SearchScope::Global, SearchScope::All] {
            let query = parsed(text)?.with_default_scope(&default_scope);
            assert_eq!(
                scopes_by_query(&query),
                [("middleware".to_owned(), SearchScope::Local)],
                "{default_scope:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn test_default_scope_local_changes_no_selection() -> Result<(), rift_error::RiftError> {
        let text = "SELECT ?symbol { \
                    SERVICE :search { ?first :query 'named'; :scope 'all'; :symbol ?symbol } \
                    SERVICE :search { ?second :query 'unnamed'; :symbol ?symbol } }";
        let parsed_scopes = scopes_by_query(&parsed(text)?);
        let defaulted = parsed(text)?.with_default_scope(&SearchScope::Local);
        assert_eq!(scopes_by_query(&defaulted), parsed_scopes);
        assert_eq!(
            parsed_scopes,
            [
                ("named".to_owned(), SearchScope::All),
                ("unnamed".to_owned(), SearchScope::Local),
            ]
        );
        Ok(())
    }

    #[test]
    fn test_default_scope_is_replaced_by_a_later_one() -> Result<(), rift_error::RiftError> {
        let query = parsed("SELECT ?hit { SERVICE :search { ?hit :query 'beacon' } }")?
            .with_default_scope(&SearchScope::Global)
            .with_default_scope(&SearchScope::All);
        assert_eq!(
            scopes_by_query(&query),
            [("beacon".to_owned(), SearchScope::All)]
        );
        Ok(())
    }

    /// The query every `:packages` test sends, with `packages` as the service's object.
    fn package_query(packages: &str) -> String {
        format!(
            "SELECT ?symbol {{ SERVICE :search {{ ?hit :query 'Serialize'; :target 'symbol'; \
             :packages {packages}; :symbol ?symbol }} }}"
        )
    }

    #[test]
    fn test_packages_read_an_rdf_json_literal_under_either_datatype_spelling()
    -> Result<(), rift_error::RiftError> {
        let entry = r#"'[{"manager":"cargo","registry":"crates.io","name":"serde"}]'"#;
        for datatype in [
            "rdf:JSON",
            "<http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON>",
        ] {
            let query = parsed(&package_query(&format!("{entry}^^{datatype}")))?
                .with_default_scope(&SearchScope::Global);
            let [selection] = query.retrieval_requests() else {
                panic!("expected one search producer");
            };
            assert_eq!(selection.request.scope, SearchScope::Global, "{datatype}");
            let [package] = selection.request.packages.as_slice() else {
                panic!("expected one requested package");
            };
            assert_eq!(package.registry.as_deref(), Some("crates.io"), "{datatype}");
        }
        Ok(())
    }

    #[test]
    fn test_packages_refuse_a_plain_string_naming_the_literal_they_take() {
        let query =
            package_query(r#"'[{"manager":"cargo","registry":"crates.io","name":"serde"}]'"#);
        let error = parsed(&query).expect_err("a plain string carries no structured value");
        assert_eq!(error.slug(), errors::search::query_syntax_invalid::SLUG);
        assert!(
            error
                .to_string()
                .contains("`:packages` takes an `rdf:JSON` literal"),
            "{error}"
        );
    }

    #[test]
    fn test_packages_refuse_an_entry_naming_no_registry() {
        for entries in [
            r#"[{"manager":"cargo","name":"serde"}]"#,
            r#"[{"manager":"npm","registry":"npmjs.org","name":"zod"},{"manager":"cargo","name":"serde"}]"#,
        ] {
            let error = parsed(&package_query(&format!("'{entries}'^^rdf:JSON")))
                .expect_err("an entry with no registry matches no release");
            assert_eq!(
                error.slug(),
                errors::search::query_syntax_invalid::SLUG,
                "{entries}"
            );
            assert!(
                error.to_string().contains("names its `registry`"),
                "{entries}: {error}"
            );
        }
    }
}
