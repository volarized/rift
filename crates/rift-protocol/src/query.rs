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

/// Default encoded answer bytes per page, reserving room for both MCP representations.
pub const QUERY_PAGE_BYTES_DEFAULT: u64 = 4_194_304;

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
#[schemars(extend("rift:since" = "v0.0.6"))]
#[schemars(extend("examples" = [
    { "query": "SELECT ?symbol ?name { ?symbol a :Symbol; :name ?name } LIMIT 20" },
    { "query": "ASK { ?symbol a :Symbol; :name 'ReadService' }", "language": "sparql" }
]))]
pub struct QueryParams {
    /// Query text.
    pub query: String,
    /// Currently only SPARQL is supported.
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
#[schemars(transform = crate::schema::declare_query_ranges)]
pub struct QueryConfiguration {
    /// Most query text bytes admitted before parsing. Must be positive.
    pub statement_size: crate::configuration::ByteSize,
    /// Most parsed algebra nodes. Must be positive.
    #[schemars(range(min = 1))]
    pub algebra_nodes: u64,
    /// Most provider requests during one acquisition. Must be positive.
    #[schemars(range(min = 1))]
    pub requests: u64,
    /// Most projected RDF statements. Must be positive.
    #[schemars(range(min = 1))]
    pub statements: u64,
    /// Most projected RDF term bytes. Must be positive.
    pub fact_size: crate::configuration::ByteSize,
    /// Most evaluated rows or triples retained. Must be positive.
    #[schemars(range(min = 1))]
    pub results: u64,
    /// Most serialized bytes retained for one answer. Must be positive.
    pub result_size: crate::configuration::ByteSize,
    /// Most rows or triples served in one transport page. Must be positive.
    #[schemars(range(min = 1))]
    pub page_size: u64,
    /// Most encoded answer bytes in one transport page. Must be positive.
    pub page_limit: crate::configuration::ByteSize,
    /// Most encoded request bytes sent to one query child. Omitted derives four times
    /// `statement_size + fact_size + result_size`, plus 33554432 bytes for framing and metadata.
    pub input_size: Option<crate::configuration::ByteSize>,
    /// Most encoded response bytes read from one query child. Omitted uses the same
    /// derivation as `input_size`, independently of an explicit `input_size`.
    pub output_size: Option<crate::configuration::ByteSize>,
    /// Most acquired record material retained by one query. Omitted uses the same
    /// derivation as `input_size`, independently of either exchange override.
    pub material_size: Option<crate::configuration::ByteSize>,
    /// Most live Rust allocation bytes in one query child. Omitted derives twice
    /// `statement_size + fact_size + result_size`, plus 33554432 bytes for initial allocations.
    pub heap_size: Option<crate::configuration::ByteSize>,
    /// Most diagnostic bytes retained from one query child's standard error.
    /// Omitted retains 8192 bytes; protocol acknowledgment bytes are counted separately.
    pub diagnostic_size: Option<crate::configuration::ByteSize>,
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
            page_limit: crate::configuration::ByteSize::from_bytes(QUERY_PAGE_BYTES_DEFAULT),
            input_size: None,
            output_size: None,
            material_size: None,
            heap_size: None,
            diagnostic_size: None,
        }
    }
}

impl QueryConfiguration {
    pub(crate) fn violation(&self) -> Option<crate::configuration::ConfigurationViolation> {
        crate::configuration::first_out_of_range([
            (
                "search.query.input_size",
                self.input_size
                    .map_or(1, crate::configuration::ByteSize::bytes),
                1,
                u64::MAX,
            ),
            (
                "search.query.output_size",
                self.output_size
                    .map_or(1, crate::configuration::ByteSize::bytes),
                1,
                u64::MAX,
            ),
            (
                "search.query.material_size",
                self.material_size
                    .map_or(1, crate::configuration::ByteSize::bytes),
                1,
                u64::MAX,
            ),
            (
                "search.query.heap_size",
                self.heap_size
                    .map_or(1, crate::configuration::ByteSize::bytes),
                1,
                u64::MAX,
            ),
            (
                "search.query.diagnostic_size",
                self.diagnostic_size
                    .map_or(1, crate::configuration::ByteSize::bytes),
                1,
                u64::MAX,
            ),
            (
                "search.query.statement_size",
                self.statement_size.bytes(),
                1,
                u64::MAX,
            ),
            (
                "search.query.algebra_nodes",
                self.algebra_nodes,
                1,
                u64::MAX,
            ),
            ("search.query.requests", self.requests, 1, u64::MAX),
            ("search.query.statements", self.statements, 1, u64::MAX),
            (
                "search.query.fact_size",
                self.fact_size.bytes(),
                1,
                u64::MAX,
            ),
            ("search.query.results", self.results, 1, u64::MAX),
            (
                "search.query.result_size",
                self.result_size.bytes(),
                1,
                u64::MAX,
            ),
            ("search.query.page_size", self.page_size, 1, u64::MAX),
            (
                "search.query.page_limit",
                self.page_limit.bytes(),
                1,
                u64::MAX,
            ),
        ])
    }
}

#[cfg(test)]
mod tests;
