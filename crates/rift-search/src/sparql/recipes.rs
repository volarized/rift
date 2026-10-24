//! Standard retrieval requests for callers migrating to the query surface.

use oxigraph::model::{Literal, NamedNode};
use rift_protocol::query::{QueryLanguage, QueryParams};
use rift_protocol::read::SearchParams;
use serde_json::Value;

use super::{QueryError, SEARCH_SERVICE, VOCABULARY, failure};

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const HIT_FIELDS: &[&str] = &["path", "unit", "start", "end", "line"];

/// Builds a SPARQL request over the existing retrieval producer.
///
/// # Errors
/// Returns a registered invalid-query error for unsupported traversal inputs,
/// an invalid retrieval limit, quantity, or an offset outside its accepted bound.
pub fn retrieval_query(parameters: &SearchParams) -> Result<QueryParams, QueryError> {
    if parameters.traversal.is_some() {
        return Err(failure::parse(
            "relationship requests must express the requested path in SPARQL",
        ));
    }
    let fields = serde_json::to_value(parameters).map_err(failure::parse)?;
    let fields = fields
        .as_object()
        .ok_or_else(|| failure::parse("retrieval parameters must serialize as an object"))?;
    let mut group = String::new();
    for (key, value) in fields {
        if key == "page_index" {
            continue;
        }
        if value.is_null() || omitted_default(key, value) {
            continue;
        }
        let predicate = NamedNode::new(format!("{VOCABULARY}{key}")).map_err(failure::parse)?;
        let object = input(value)?;
        group.push_str(&format!("?hit {predicate} {object} . "));
    }
    group.push_str("?hit :index ?index . OPTIONAL { ?hit :symbol ?symbol . OPTIONAL { ?symbol :name ?name } OPTIONAL { ?symbol :kind ?kind } OPTIONAL { ?symbol :language ?language } } ");
    let mut projected = String::from("?hit ?symbol ?name ?kind ?language ?index");
    for field in HIT_FIELDS {
        group.push_str(&format!("OPTIONAL {{ ?hit :{field} ?{field} }} "));
        projected.push_str(&format!(" ?{field}"));
    }
    for include in parameters.include.as_ref().into_iter().flatten() {
        let field = match include {
            rift_protocol::read::SearchInclude::Source => "source",
            rift_protocol::read::SearchInclude::Score => "score",
        };
        group.push_str(&format!("OPTIONAL {{ ?hit :{field} ?{field} }} "));
        projected.push_str(&format!(" ?{field}"));
    }
    let slice = match parameters.limit {
        Some(limit) => {
            if limit == 0 || limit > rift_protocol::read::PAGE_LIMIT_MAX {
                return Err(failure::parse(
                    "retrieval limit is outside its accepted bound",
                ));
            }
            let offset = parameters
                .page_index
                .checked_mul(limit)
                .ok_or_else(|| failure::parse("query offset exceeds its bound"))?;
            format!(" LIMIT {limit} OFFSET {offset}")
        }
        None if parameters.page_index == 0 => String::new(),
        None => return Err(failure::parse("query offset requires a selected limit")),
    };
    let query = format!(
        "PREFIX : <{VOCABULARY}> SELECT {projected} {{ SERVICE <{SEARCH_SERVICE}> {{ {group} }} }} ORDER BY ?index{slice}"
    );
    Ok(QueryParams {
        query,
        language: QueryLanguage::Sparql,
    })
}

fn input(value: &Value) -> Result<Literal, QueryError> {
    match value {
        Value::String(value) => Ok(Literal::from(value.as_str())),
        Value::Number(value) => {
            Ok(Literal::from(value.as_u64().ok_or_else(|| {
                failure::parse("retrieval quantity must be an unsigned integer")
            })?))
        }
        Value::Bool(value) => Ok(Literal::from(*value)),
        _ => Ok(Literal::new_typed_literal(
            serde_json::to_string(value).map_err(failure::parse)?,
            NamedNode::new(RDF_JSON).map_err(failure::parse)?,
        )),
    }
}

fn omitted_default(key: &str, value: &Value) -> bool {
    matches!((key, value), ("target", Value::String(value)) if value == "all")
        || matches!((key, value), ("order", Value::String(value)) if value == "relevance")
        || matches!((key, value), ("scope", Value::String(value)) if value == "local")
        || (key == "page_index" && value.as_u64() == Some(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sparql::{PreparedQuery, QueryLimits};
    use spargebra::Query;
    use spargebra::algebra::GraphPattern;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn limits() -> QueryLimits {
        QueryLimits {
            query_bytes_max: 16_384,
            algebra_nodes_max: 4096,
            quads_max: 1,
            rdf_bytes_max: 1,
            result_items_max: 1,
            result_bytes_max: 1,
        }
    }

    #[test]
    fn recipe_preserves_literal_bytes_and_typed_path_selection() -> TestResult {
        for pattern in ["TODO|FIXME", r#"say\("hi"\)"#, "λ\\w+", "first\nsecond\t"] {
            let producer: SearchParams = serde_json::from_value(serde_json::json!({
                "pattern":pattern,
                "target":"file",
                "paths":{"include":["src/**","**/say\"hi\".rs"],"exclude":["generated/**"]}
            }))?;
            let recipe = retrieval_query(&producer)?;
            let wire = serde_json::to_value(&recipe)?;
            assert_eq!(wire.as_object().ok_or("query object")?.len(), 2);
            assert_eq!(wire["language"], "sparql");
            let parsed = PreparedQuery::parse(&recipe.query, limits())?;
            let requests = parsed.retrievals();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].pattern.as_deref(), Some(pattern));
            assert_eq!(requests[0].target, producer.target);
            assert_eq!(requests[0].paths, producer.paths);
        }
        Ok(())
    }

    #[test]
    fn recipe_uses_outer_slicing_and_keeps_producer_page_zero() -> TestResult {
        let producer: SearchParams = serde_json::from_value(serde_json::json!({
            "pattern":"x","limit":50,"page_index":2
        }))?;
        let recipe = retrieval_query(&producer)?;
        let parsed = spargebra::SparqlParser::new().parse_query(&recipe.query)?;
        let Query::Select {
            pattern: GraphPattern::Slice { start, length, .. },
            ..
        } = parsed
        else {
            return Err("recipe must carry the requested outer slice".into());
        };
        assert_eq!((start, length), (100, Some(50)));
        let prepared = PreparedQuery::parse(&recipe.query, limits())?;
        assert_eq!(prepared.retrievals()[0].page_index, 0);
        assert_eq!(prepared.retrievals()[0].limit, Some(50));
        let unbounded: SearchParams = serde_json::from_value(serde_json::json!({"pattern":"x"}))?;
        let recipe = retrieval_query(&unbounded)?;
        let Query::Select { pattern, .. } =
            spargebra::SparqlParser::new().parse_query(&recipe.query)?
        else {
            return Err("recipe must select bindings".into());
        };
        assert!(!matches!(pattern, GraphPattern::Slice { .. }));
        Ok(())
    }

    #[test]
    fn recipe_refuses_invalid_limit_and_offset_before_emitting_a_request() -> TestResult {
        for fields in [
            serde_json::json!({"pattern":"x","limit":0}),
            serde_json::json!({"pattern":"x","limit":10_001}),
            serde_json::json!({"pattern":"x","limit":2,"page_index":u64::MAX}),
            serde_json::json!({"pattern":"x","page_index":1}),
        ] {
            assert!(retrieval_query(&serde_json::from_value(fields)?).is_err());
        }
        Ok(())
    }
}
