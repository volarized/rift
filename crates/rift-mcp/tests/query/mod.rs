//! Standard SPARQL requests and typed projected answers for live MCP suites.

#![allow(dead_code, reason = "shared query helpers serve several public suites")]

use std::error::Error;
use std::fmt::Write as _;

use oxrdf::{Literal, NamedNode, NamedNodeRef};
use rift_protocol::query::{QueryLanguage, QueryParams};
use rift_protocol::read::SearchParams;
use serde_json::Value;

type TestResult<T> = Result<T, Box<dyn Error>>;

const RDF_JSON: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#JSON";
const HIT_FIELDS: &[&str] = &["path", "unit", "start", "end", "line"];

/// Exact published declaration facts, independent of retrieval ranking readiness.
pub(crate) fn declaration(name: &str, path: &str) -> QueryParams {
    let name = Literal::from(name);
    let unit = rift_protocol::identity::encode_source_unit_key(path);
    QueryParams {
        query: format!(
            "SELECT ?symbol ?name ?kind ?language ?unit ?source ?start ?end {{ ?symbol a :Symbol; :name {name}; :name ?name; :kind ?kind; :language ?language . ?node :of ?symbol; :index 0; :source ?source; :range [ :unit <rift://source/project/{unit}>; :unit ?unit; :start ?start; :end ?end ] }}"
        ),
        language: QueryLanguage::Sparql,
    }
}

/// Projects existing producer results through the public SPARQL surface.
pub(crate) fn retrieval(parameters: &SearchParams) -> TestResult<QueryParams> {
    projected_retrieval(parameters, "", "")
}

/// Adds explicit projected fields and graph patterns to an existing retrieval request.
pub(crate) fn projected_retrieval(
    parameters: &SearchParams,
    projection: &str,
    pattern: &str,
) -> TestResult<QueryParams> {
    if parameters.traversal.is_some() {
        return Err("relationship tests must express the requested path in SPARQL".into());
    }
    let fields = serde_json::to_value(parameters)?;
    let fields = fields
        .as_object()
        .ok_or("retrieval parameters must serialize as an object")?;
    let mut group = String::new();
    for (key, value) in fields {
        if value.is_null() || omitted_default(key, value) {
            continue;
        }
        let key = if key == "revision" { "rev" } else { key };
        let predicate = NamedNode::new(format!(
            "{}{key}",
            rift_search::sparql::VOCABULARY_NAMESPACE
        ))?;
        let object = input(value)?;
        write!(group, "?hit {predicate} {object} . ")?;
    }
    group.push_str("?hit :index ?index . OPTIONAL { ?hit :symbol ?symbol . OPTIONAL { ?symbol :name ?name } OPTIONAL { ?symbol :kind ?kind } OPTIONAL { ?symbol :language ?language } } ");
    let mut projected = String::from("?hit ?symbol ?name ?kind ?language ?index");
    for field in HIT_FIELDS {
        write!(group, "OPTIONAL {{ ?hit :{field} ?{field} }} ")?;
        write!(projected, " ?{field}")?;
    }
    for include in parameters.include.as_ref().into_iter().flatten() {
        let field = match include {
            rift_protocol::read::SearchInclude::Source => "source",
            rift_protocol::read::SearchInclude::Score => "score",
        };
        write!(group, "OPTIONAL {{ ?hit :{field} ?{field} }} ")?;
        write!(projected, " ?{field}")?;
    }
    group.push_str(pattern);
    projected.push(' ');
    projected.push_str(projection);
    let query = format!("SELECT {projected} {{ SERVICE :search {{ {group} }} }} ORDER BY ?index");
    Ok(QueryParams {
        query,
        language: QueryLanguage::Sparql,
    })
}

/// Selects a page of an existing execution without querying current facts.
pub(crate) fn retained_page(answer: &Value, page: u64) -> TestResult<String> {
    let view = serde_json::from_value(answer["view"]["id"].clone())?;
    let result = serde_json::from_value(answer["answer"].clone())?;
    Ok(rift_search::sparql::continuation_query(
        &result, &view, page,
    )?)
}

/// Projects endpoints of a path from one canonical symbol.
pub(crate) fn path(seed: &str, expression: &str) -> TestResult<QueryParams> {
    let seed = NamedNodeRef::new(seed)?;
    Ok(QueryParams {
        query: format!(
            "SELECT DISTINCT ?symbol {{ {seed} {expression} ?symbol }} ORDER BY ?symbol"
        ),
        language: QueryLanguage::Sparql,
    })
}

/// The projected rows of an actual `SELECT` answer.
pub(crate) fn rows(answer: &Value) -> TestResult<&[Value]> {
    if answer["answer"]["result"] != "bindings" {
        return Err(format!("expected SELECT bindings: {answer}").into());
    }
    answer["answer"]["bindings"]
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| "SELECT bindings must be an array".into())
}

/// A projected canonical identity, with its RDF term kind checked.
pub(crate) fn iri<'a>(row: &'a Value, field: &str) -> TestResult<&'a str> {
    if row[field]["type"] != "iri" {
        return Err(format!("{field} must be an IRI: {row}").into());
    }
    row[field]["value"]
        .as_str()
        .ok_or_else(|| format!("{field} must carry its IRI value").into())
}

/// A projected literal, preserving its datatype in the original answer.
pub(crate) fn literal<'a>(row: &'a Value, field: &str) -> TestResult<&'a str> {
    if !matches!(
        row[field]["type"].as_str(),
        Some("literal" | "language_literal")
    ) {
        return Err(format!("{field} must be a literal: {row}").into());
    }
    row[field]["value"]
        .as_str()
        .ok_or_else(|| format!("{field} must carry its literal value").into())
}

fn input(value: &Value) -> TestResult<Literal> {
    match value {
        Value::String(value) => Ok(Literal::from(value.as_str())),
        Value::Number(value) => Ok(Literal::from(
            value
                .as_u64()
                .ok_or("retrieval quantity must be an unsigned integer")?,
        )),
        Value::Bool(value) => Ok(Literal::from(*value)),
        _ => Ok(Literal::new_typed_literal(
            serde_json::to_string(value)?,
            NamedNode::new(RDF_JSON)?,
        )),
    }
}

fn omitted_default(key: &str, value: &Value) -> bool {
    matches!((key, value), ("target", Value::String(value)) if value == "all")
        || matches!((key, value), ("order", Value::String(value)) if value == "relevance")
        || matches!((key, value), ("scope", Value::String(value)) if value == "local")
        || (key == "page_index" && value.as_u64() == Some(0))
}
