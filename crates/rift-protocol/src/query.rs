//! Query requests and complete fact records shared by search adapters.

mod answer;

pub use answer::{QueryAnswer, RdfTerm, RdfTriple};

use crate::read::{
    Pagination, ReadWarning, RelationshipFacet, SourceUnitId, SourceUnitSpan, Symbol, SymbolId,
};
use crate::search_read::SearchUnavailable;
use crate::symbol_read::{CapturedView, SymbolDeclaration};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Most encoded answer bytes per page, reserving room for both MCP representations.
pub const QUERY_PAGE_BYTES_MAX: u64 = 4_194_304;

const STATEMENT_BYTES_MIN: u64 = 1_024;
const STATEMENT_BYTES_MAX: u64 = 1_048_576;
const ALGEBRA_NODES_MAX: u64 = 65_536;
const REQUESTS_MAX: u64 = 100_000;
const STATEMENTS_MAX: u64 = 10_000_000;
const FACT_BYTES_MIN: u64 = 1_048_576;
const FACT_BYTES_MAX: u64 = 4_294_967_296;
const RESULT_ITEMS_MAX: u64 = 1_000_000;
const RESULT_BYTES_MIN: u64 = 1_024;
const RESULT_BYTES_MAX: u64 = 1_073_741_824;
const PAGE_ITEMS_MAX: u64 = 10_000;
const PAGE_BYTES_MIN: u64 = 1_024;

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
#[schemars(extend("examples" = [
    { "query": "SELECT ?symbol ?name { ?symbol a :Symbol; :name ?name } LIMIT 20" },
    { "query": "ASK { ?symbol a :Symbol; :name 'ReadService' }", "language": "sparql" }
]))]
pub struct QueryParams {
    /// SPARQL Query text. The server supplies vocabulary and standard prefixes.
    pub query: String,
    /// The implemented query language. Omitted selects SPARQL.
    #[serde(default)]
    pub language: QueryLanguage,
}

/// One physical reference or call supplied by a selected provider.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryOccurrence {
    /// The canonical symbol referenced at this location.
    pub target: SymbolId,
    /// The enclosing declaration, when the provider can establish it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enclosing: Option<SymbolId>,
    /// The exact source unit and UTF-8 byte range of the occurrence.
    pub source: SourceUnitSpan,
    /// The relationship established by this occurrence.
    pub facet: RelationshipFacet,
}

/// A symbol together with the physical declarations selected for this read.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuerySymbol {
    /// The logical symbol's current selected facts.
    pub symbol: Symbol,
    /// Physical declarations, preserving their association indices.
    #[schemars(length(max = 10000))]
    pub declarations: Vec<SymbolDeclaration>,
}

/// Selected source bytes, when their projection was requested.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuerySource {
    /// The physical source owner and path.
    pub unit: SourceUnitId,
    /// Full digest of the selected physical bytes, including when text is unrequested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<crate::identity::SourceDigest>,
    /// Selected text, absent when source is unavailable or unrequested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 67_108_864))]
    pub text: Option<String>,
}

/// One retained page of bindings, a boolean, or RDF triples.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryResult {
    /// The result form selected by the query.
    pub answer: QueryAnswer,
    /// The immutable serving context shared by later exact reads and continuation.
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

/// The `[search.query]` bounds for acquisition, evaluation and retained answers.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueryConfiguration {
    /// Most query text admitted before parsing, 1kb to 1mb.
    pub statement_size: crate::configuration::ByteSize,
    /// Most parsed algebra nodes, 1 to 65536.
    #[schemars(range(min = 1, max = 65_536))]
    pub algebra_nodes: u64,
    /// Most provider requests during one acquisition, 1 to 100000.
    #[schemars(range(min = 1, max = 100_000))]
    pub requests: u64,
    /// Most projected RDF statements, 1 to 10000000.
    #[schemars(range(min = 1, max = 10_000_000))]
    pub statements: u64,
    /// Most projected RDF term bytes, 1mb to 4gb.
    pub fact_size: crate::configuration::ByteSize,
    /// Most evaluated rows or triples retained, 1 to 1000000.
    #[schemars(range(min = 1, max = 1_000_000))]
    pub results: u64,
    /// Most serialized bytes retained for one answer, 1kb to 1gb.
    pub result_size: crate::configuration::ByteSize,
    /// Most rows or triples served in one transport page, 1 to 10000.
    #[schemars(range(min = 1, max = 10_000))]
    pub page_size: u64,
    /// Most encoded answer bytes in one transport page, 1kb to 4mb.
    pub page_limit: crate::configuration::ByteSize,
}

impl Default for QueryConfiguration {
    fn default() -> Self {
        Self {
            statement_size: crate::configuration::ByteSize::from_bytes(65_536),
            algebra_nodes: 8_192,
            requests: 20_000,
            statements: 2_000_000,
            fact_size: crate::configuration::ByteSize::from_bytes(268_435_456),
            results: 1_000_000,
            result_size: crate::configuration::ByteSize::from_bytes(134_217_728),
            page_size: 1_000,
            page_limit: crate::configuration::ByteSize::from_bytes(QUERY_PAGE_BYTES_MAX),
        }
    }
}

impl QueryConfiguration {
    pub(crate) fn violation(&self) -> Option<crate::configuration::ConfigurationViolation> {
        crate::configuration::first_out_of_range([
            (
                "search.query.statement_size",
                self.statement_size.bytes(),
                STATEMENT_BYTES_MIN,
                STATEMENT_BYTES_MAX,
            ),
            (
                "search.query.algebra_nodes",
                self.algebra_nodes,
                1,
                ALGEBRA_NODES_MAX,
            ),
            ("search.query.requests", self.requests, 1, REQUESTS_MAX),
            (
                "search.query.statements",
                self.statements,
                1,
                STATEMENTS_MAX,
            ),
            (
                "search.query.fact_size",
                self.fact_size.bytes(),
                FACT_BYTES_MIN,
                FACT_BYTES_MAX,
            ),
            ("search.query.results", self.results, 1, RESULT_ITEMS_MAX),
            (
                "search.query.result_size",
                self.result_size.bytes(),
                RESULT_BYTES_MIN,
                RESULT_BYTES_MAX,
            ),
            ("search.query.page_size", self.page_size, 1, PAGE_ITEMS_MAX),
            (
                "search.query.page_limit",
                self.page_limit.bytes(),
                PAGE_BYTES_MIN,
                QUERY_PAGE_BYTES_MAX,
            ),
        ])
    }
}

#[cfg(test)]
mod tests;
