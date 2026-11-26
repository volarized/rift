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
    {
        "query": "SELECT ?symbol ?name { ?symbol a :Symbol; :name ?name } LIMIT 20"
    },
    {
        "query": "ASK { ?symbol a :Symbol; :name 'ReadService' }",
        "language": "sparql"
    },
    {
        "query": "SELECT ?edge { ?caller :calls ?callee . ?edge a :Edge; :from ?caller; :to ?callee }",
        "include": { "providers": true }
    }
]))]
pub struct QueryParams {
    /// Attaches the sorted analyses that produced each returned edge.
    #[serde(default, skip_serializing_if = "QueryInclude::is_empty")]
    pub include: QueryInclude,
    /// Query text in `language`. A SPARQL query uses `SERVICE :search` for text and regex
    /// retrieval and `SERVICE :flows` for call paths around ranked symbols.
    pub query: String,
    /// The language of `query`. Omitted, `sparql`, the only language the server evaluates.
    #[serde(default)]
    pub language: QueryLanguage,
    /// The 1-based page to read.
    #[serde(default = "default_page")]
    #[schemars(range(min = 1_u64))]
    pub page: u64,
    /// Answer rows per page, at most 1000.
    #[serde(default = "default_max_results")]
    #[schemars(range(min = 1_u64, max = QUERY_MAX_RESULTS))]
    pub max_results: u64,
    /// The indexed revision to read. Omitted reads the served revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<crate::revision::Revision>,
}

/// Optional projections of a query answer. An omitted member is `false`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(example = serde_json::json!({"providers": true}))]
pub struct QueryInclude {
    /// Attaches `providers` to merged edge terms and call steps. Explicit SPARQL
    /// selections and filters on `:provider` retain the stored producer facts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub providers: bool,
}

impl QueryInclude {
    /// Whether the caller requested no projection.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.providers
    }
}

/// Most answer rows in one query page.
pub const QUERY_MAX_RESULTS: u64 = 1000;
fn default_page() -> u64 {
    crate::read::PAGE_DEFAULT
}
fn default_max_results() -> u64 {
    crate::read::MAX_RESULTS_DEFAULT
}
impl Default for QueryParams {
    fn default() -> Self {
        Self {
            query: String::new(),
            include: QueryInclude::default(),
            language: QueryLanguage::default(),
            page: default_page(),
            max_results: default_max_results(),
            revision: None,
        }
    }
}

/// One page of bindings, a boolean, or RDF triples.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryResult {
    /// The result form selected by the query.
    pub answer: QueryAnswer,
    /// The captured read and its expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<CapturedView>,
    /// The selected page of answer rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pagination: Option<Pagination>,
    /// The indexed revision this answer read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<crate::revision::Revision>,
    /// Coverage and discovery limits affecting the query result. An answer read from partly
    /// covered facts carries the warnings that name what is missing, and every row it returns
    /// stands. For `SERVICE :flows`, `traversal_truncated` reports the visited symbol count and
    /// names the bound in `detail`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ReadWarning>,
}

/// A complete query answer or the typed cause preventing evaluation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
#[schemars(transform = crate::schema::require_result_object)]
#[schemars(extend("examples" = [
    {
        "answer": {
            "result": "bindings",
            "variables": [
                "name"
            ],
            "bindings": [
                {
                    "name": {
                        "type": "literal",
                        "value": "ReadService",
                        "datatype": "http://www.w3.org/2001/XMLSchema#string"
                    }
                }
            ]
        },
        "pagination": {
            "page": 1,
            "pages": 1,
            "max_results": 10,
            "total": 1
        }
    },
    {
        "answer": {
            "result": "boolean",
            "value": true
        }
    },
    {
        "answer": {
            "result": "graph",
            "triples": []
        },
        "pagination": {
            "page": 1,
            "pages": 0,
            "max_results": 10,
            "total": 0
        }
    },
    {
        "outcome": "unavailable",
        "reason": "view_context"
    }
]))]
pub enum QueryReadResult {
    /// An answer evaluated over every required fact.
    Results(Box<QueryResult>),
    /// Required facts or retained state cannot be established.
    Unavailable(SearchUnavailable),
}

/// Views one server retains when `[search.query] retained_views` is omitted.
pub const QUERY_RETAINED_VIEWS_DEFAULT: u64 = 256;
/// Most views `[search.query] retained_views` accepts.
pub const QUERY_RETAINED_VIEWS_MAX: u64 = 4096;
/// Retained view material when `[search.query] retained_size` is omitted: 8 GiB.
pub const QUERY_RETAINED_BYTES_DEFAULT: u64 = 8 << 30;
/// Least retained view material `[search.query] retained_size` accepts: 1 MiB.
pub const QUERY_RETAINED_BYTES_MIN: u64 = 1 << 20;
/// Most retained view material `[search.query] retained_size` accepts: 64 GiB.
pub const QUERY_RETAINED_BYTES_MAX: u64 = 64 << 30;

/// Local symbol inventory selection and bounds on query results and retained views.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_query_ranges)]
pub struct QueryConfiguration {
    /// Whether graph queries can read the captured local symbol inventory. Defaults to true.
    pub local_symbol_inventory: bool,
    /// Most encoded bytes on one page, from 1 KiB to 4 MiB.
    pub page_limit: crate::configuration::ByteSize,
    /// Most views the server retains for later pages, 1 to 4096. A new view past
    /// the bound releases the least recently used one.
    #[schemars(range(min = 1, max = QUERY_RETAINED_VIEWS_MAX))]
    pub retained_views: u64,
    /// Most material kept alive by retained views, 1mb to 64gb. Counts query results,
    /// capture metadata, and index data kept alive beyond the current publication.
    /// A capture or publication past the bound releases the least recently used views.
    /// A new view whose own charge exceeds the bound is refused.
    pub retained_size: crate::configuration::ByteSize,
}

impl Default for QueryConfiguration {
    fn default() -> Self {
        Self {
            local_symbol_inventory: true,
            page_limit: crate::configuration::ByteSize::from_bytes(4 * 1024 * 1024),
            retained_views: QUERY_RETAINED_VIEWS_DEFAULT,
            retained_size: crate::configuration::ByteSize::from_bytes(QUERY_RETAINED_BYTES_DEFAULT),
        }
    }
}

impl QueryConfiguration {
    pub(crate) fn violation(&self) -> Option<crate::configuration::ConfigurationViolation> {
        crate::configuration::first_out_of_range([
            (
                "search.query.page_limit",
                self.page_limit.bytes(),
                1024,
                4 * 1024 * 1024,
            ),
            (
                "search.query.retained_views",
                self.retained_views,
                1,
                QUERY_RETAINED_VIEWS_MAX,
            ),
            (
                "search.query.retained_size",
                self.retained_size.bytes(),
                QUERY_RETAINED_BYTES_MIN,
                QUERY_RETAINED_BYTES_MAX,
            ),
        ])
    }
}
