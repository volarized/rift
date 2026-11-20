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
    /// Coverage and discovery limits affecting the query result. For `SERVICE :flows`,
    /// `traversal_truncated` reports the visited symbol count and names the bound in `detail`.
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

/// Bounds on one query result page.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueryConfiguration {
    /// Most encoded bytes on one page, from 1 KiB to 4 MiB.
    pub page_limit: crate::configuration::ByteSize,
}

impl Default for QueryConfiguration {
    fn default() -> Self {
        Self {
            page_limit: crate::configuration::ByteSize::from_bytes(4 * 1024 * 1024),
        }
    }
}

impl QueryConfiguration {
    pub(crate) fn violation(&self) -> Option<crate::configuration::ConfigurationViolation> {
        crate::configuration::first_out_of_range([(
            "search.query.page_limit",
            self.page_limit.bytes(),
            1024,
            4 * 1024 * 1024,
        )])
    }
}
