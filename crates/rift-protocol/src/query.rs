//! Requests and retained result forms for the search tool.

mod answer;

pub use answer::{QueryAnswer, RdfTerm, RdfTriple};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::read::{Pagination, ReadWarning};
use crate::search_read::SearchUnavailable;
use crate::symbol_read::CapturedView;

/// The query language the server evaluates.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryLanguage {
    /// SPARQL 1.1 Query over the selected codebase facts.
    #[default]
    Sparql,
}

/// A query over the accepted workspace and explicitly selected datasets.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.73"))]
#[schemars(extend("examples" = [
    { "query": "SELECT ?symbol ?name { ?symbol a :Symbol; :name ?name } LIMIT 20" },
    { "query": "ASK { ?symbol a :Symbol; :name 'ReadService' }", "language": "sparql" }
]))]
pub struct QueryParams {
    /// Query text.
    pub query: String,
    /// Currently, only SPARQL is supported.
    #[serde(default)]
    pub language: QueryLanguage,
}

/// One retained page of bindings, a boolean, or RDF triples.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryResult {
    /// The result form selected by the query.
    pub answer: QueryAnswer,
    /// The retained execution and its original expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<CapturedView>,
    /// The transport page within this retained execution.
    pub pagination: QueryPagination,
    /// Conditions the caller must consider when reading this answer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ReadWarning>,
}

/// A complete query answer or the typed cause preventing evaluation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
#[schemars(transform = crate::schema::require_query_result_object)]
#[schemars(extend("examples" = [
    {
        "answer": { "result": "bindings", "variables": ["name"], "bindings": [
            { "name": { "type": "literal", "value": "ReadService", "datatype": "http://www.w3.org/2001/XMLSchema#string" } }
        ] },
        "pagination": { "page_index": 0, "total_pages": 1 }
    },
    { "answer": { "result": "boolean", "value": true }, "pagination": { "page_index": 0, "total_pages": 1 } },
    { "answer": { "result": "graph", "triples": [] }, "pagination": { "page_index": 0, "total_pages": 0 } },
    { "outcome": "unavailable", "reason": "view_context", "warnings": [] }
]))]
pub enum QueryReadResult {
    /// An answer evaluated over every required fact.
    Results(Box<QueryResult>),
    /// Required facts or retained state cannot be established.
    Unavailable(SearchUnavailable),
}

/// The selected transport page and its continuation query.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryPagination {
    /// The page index and count within the original retained execution.
    #[serde(flatten)]
    pub page: Pagination,
    /// A query selecting the next retained page, absent after the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_query: Option<String>,
}

impl From<Pagination> for QueryPagination {
    fn from(page: Pagination) -> Self {
        Self {
            page,
            next_query: None,
        }
    }
}
/// Bounds on one transport page of a retained query result.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueryConfiguration {
    /// Most bindings or triples on one page, from 1 to 1000.
    #[schemars(range(min = 1, max = 1000))]
    pub page_size: u64,
    /// Most encoded bytes on one page, from 1 KiB to 4 MiB.
    pub page_limit: crate::configuration::ByteSize,
}

impl Default for QueryConfiguration {
    fn default() -> Self {
        Self {
            page_size: 1000,
            page_limit: crate::configuration::ByteSize::from_bytes(4 * 1024 * 1024),
        }
    }
}

impl QueryConfiguration {
    pub(crate) fn violation(&self) -> Option<crate::configuration::ConfigurationViolation> {
        crate::configuration::first_out_of_range([
            ("search.query.page_size", self.page_size, 1, 1000),
            (
                "search.query.page_limit",
                self.page_limit.bytes(),
                1024,
                4 * 1024 * 1024,
            ),
        ])
    }
}
