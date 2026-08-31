//! Wire models for the `search` MCP tool: request criteria and result hits. Extracted from
//! [`crate::read`] so that module stays below its size bound; every type here is re-exported
//! from `read` so existing `rift_protocol::read::SearchParams`-style paths keep resolving.

use crate::read::{
    Language, NodeId, PAGE_INDEX_DEFAULT, Pagination, ProjectPath, ReadWarning, Relationship,
    RelationshipFacet, RevisionId, Symbol, SymbolId, TextRange,
};
use crate::schema;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::VariantArray;

/// One auditable step in a search traversal. `relationship` retains source-node evidence and
/// derivation; `direction` records how the walk followed it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GraphHop {
    /// The relationship followed for this step.
    pub relationship: Relationship,
    /// How the walk followed the directed relationship.
    pub direction: HopDirection,
}

/// How one path step followed its directed relationship.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum HopDirection {
    /// The walk followed the edge from source to target.
    Outgoing,
    /// The walk followed the edge against its direction, from target to source.
    Incoming,
}

/// Which indexed field produced a search hit.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum MatchedField {
    /// The declaration's name matched.
    Name,
    /// A rendered signature matched.
    Signature,
    /// An attached doc comment matched.
    Documentation,
    /// The file's contents matched.
    Content,
    /// The ranked lane placed the hit; no field match proves the query's literal bytes
    /// appear. The lane ranks whether or not `[search.semantic]` is enabled, so this member
    /// names the lane rather than the tier that may or may not have contributed to it.
    Ranked,
    /// The project-relative path matched.
    Path,
    /// A relationship traversal reached the hit.
    Relationship,
}

/// Project-relative glob using *, ?, **, and character classes. Forward-slash separated on
/// every platform, whatever separator the host OS uses natively.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct PathPattern(
    #[schemars(example = &"src/**")]
    #[schemars(regex(
        pattern = r"^(?!/)(?!\.\.?(/|$))(?!.*(/\.\.?)(/|$))[^\\\u0000-\u001F\u007F]+$"
    ))]
    pub String,
);

impl PathPattern {
    /// Classifies this pattern against the forward-slash-only contract [`PathPattern`]
    /// advertises. `schemars` regexes are declarative only - nothing enforces them at
    /// runtime - so every acceptance point calls this before the pattern reaches a glob
    /// engine, where a stray backslash would otherwise be read as an escape.
    #[must_use]
    pub fn violation(&self) -> Option<PathPatternViolation> {
        path_pattern_violation(&self.0)
    }
}

/// Reason a path pattern breaks the forward-slash-only contract every glob list enforces.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathPatternViolation {
    /// Pattern is empty.
    Empty,
    /// Pattern starts with `/`.
    Absolute,
    /// Pattern contains a `\` byte; backslash is never treated as an escape or separator.
    Backslash,
    /// Pattern contains an ASCII control character.
    ControlCharacter,
    /// A `/`-separated segment is `.` or `..`.
    DotSegment,
}

impl PathPatternViolation {
    /// This violation's wire spelling, equal to its `Serialize` output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Absolute => "absolute",
            Self::Backslash => "backslash",
            Self::ControlCharacter => "control_character",
            Self::DotSegment => "dot_segment",
        }
    }
}

/// Classifies one path-pattern value against the rules [`PathPattern`]'s schema advertises.
/// Arms are ordered by precedence: the first matching rule names the violation.
pub(crate) fn path_pattern_violation(value: &str) -> Option<PathPatternViolation> {
    match value.as_bytes() {
        [] => Some(PathPatternViolation::Empty),
        [b'/', ..] => Some(PathPatternViolation::Absolute),
        bytes if bytes.contains(&b'\\') => Some(PathPatternViolation::Backslash),
        _ if value.chars().any(char::is_control) => Some(PathPatternViolation::ControlCharacter),
        _ if value.split('/').any(is_dot_segment) => Some(PathPatternViolation::DotSegment),
        _ => None,
    }
}

fn is_dot_segment(segment: &str) -> bool {
    matches!(segment, "." | "..")
}

/// Which files a query runs over, as three lists of globs matched against the project-relative
/// path. The same glob engine backs the workspace's `[source]` policy. `include: ["src/**"]`
/// selects the source tree; `exclude: ["src/generated/**"]` then removes generated output.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PathSelector {
    /// Globs a path has to match to be searched at all. Empty includes every visible file.
    #[serde(default)]
    pub include: Vec<PathPattern>,
    /// Globs that drop a path `include` already matched.
    #[serde(default)]
    pub exclude: Vec<PathPattern>,
    /// Globs reaching files the workspace's `[source]` policy or `.gitignore` excluded from
    /// the index. Matches are bounded per request, and the server refuses the search when the
    /// bound is crossed rather than truncating it silently.
    #[serde(default)]
    pub force_include: Vec<PathPattern>,
}

/// The total order a paginated answer comes back in. Every order ends in the result's own
/// identity, so two results that tie never swap places between pages.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ResultOrder {
    /// Best score first, with identity breaking ties.
    Relevance,
    /// Project path order, with identity breaking ties.
    Path,
    /// The result's own identity alone.
    Identity,
}

/// One search hit. Its file, node, or symbol payload carries the canonical identity.
/// Dependency and synthetic symbols can have no readable source; node and file hits cannot.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::pair_range_with_line)]
#[schemars(transform = schema::declare_search_hit_empty_defaults)]
pub struct SearchHit {
    /// What was found. A symbol, a node, or a file - whichever `target` allowed.
    pub hit: SearchHitTarget,
    /// How well this hit matched, used to order the page and merge duplicate hits.
    /// Present on the wire when `include` names `score`; comparable within one answer
    /// and nowhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// Which indexed fields produced the match. Absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_by: Vec<MatchedField>,
    /// The source text around the hit, requested with `include: ["source"]`. Covers the
    /// hit's `range`; a caller that needs the range already has it there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Byte range of the hit within `path`. Absent for a symbol whose source is
    /// unavailable or synthetic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<TextRange>,
    /// The 1-based source line where the hit begins, or absent with `range`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1_u64))]
    pub line: Option<u64>,
    /// Project-relative path of the hit, where the location is a project path. Absent for
    /// a hit whose only location is a dependency or standard-library source unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<ProjectPath>,
    /// Shortest relationship path to this hit, present when a traversal reached it,
    /// including a hit also matched lexically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 2))]
    pub traversal_path: Option<Vec<GraphHop>>,
    /// Number of edges in `traversal_path`. It is present exactly when `traversal_path` is
    /// present and equals its length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1_u64, max = 2_u64))]
    pub distance: Option<u64>,
}

/// What a search hit is. Tagged, so the payload correlation survives code generation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "target", deny_unknown_fields, rename_all = "snake_case")]
#[schemars(transform = schema::declare_search_hit_target_file_empty_defaults)]
pub enum SearchHitTarget {
    /// A symbol hit: the declaration a provider resolved.
    Symbol {
        /// The declaration that matched.
        symbol: Box<Symbol>,
    },
    /// A node hit: one place in a syntax tree, without its enclosing symbol record.
    Node {
        /// The syntax-tree node's identity, the full edit address `replace_node` accepts.
        node: NodeId,
    },
    /// A file hit: one entry of the tree, whether or not any provider reads it.
    File {
        /// Size in bytes.
        #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
        size: u64,
        /// Distinct `Language` values that read this file, sorted by name and dialect.
        /// Absent when empty.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        languages: Vec<Language>,
    },
}

/// Extra payload to attach to every hit. Every entry costs response bytes per hit, so the
/// caller requests only what it will read.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SearchInclude {
    /// The source text around each hit.
    Source,
    /// The ranking value used to order the page.
    Score,
}

/// Criteria for one search. The caller supplies a lexical `query`, a relationship
/// `traversal`, or both; `paths` narrows the files eligible for either.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.6"))]
#[schemars(extend("examples" = [
    {
        "target": "all",
        "order": "relevance",
        "query": "load_config",
        "paths": {
            "include": [
                "src/**"
            ],
            "exclude": [],
            "force_include": []
        },
        "include": [
            "source"
        ],
        "limit": 20,
        "page_index": 0
    },
    {
        "target": "symbol",
        "query": "load_config",
        "order": "path",
        "limit": 10
    },
    {
        "target": "symbol",
        "traversal": {
            "seed": "rift://symbol/rust/crates/rift-server/src/read.rs/ReadService",
            "direction": "incoming",
            "depth": 2,
            "facets": [
                "calls",
                "references",
                "imports"
            ]
        },
        "limit": 25
    }
]))]
pub struct SearchParams {
    /// Which entity kinds may be returned - a kind selector, never the text to search for;
    /// that is `query`. Omitted, every kind may match.
    #[serde(default = "default_search_params_target")]
    pub target: SearchParamsTarget,
    /// Which total order the page comes back in. Omitted, relevance.
    #[serde(default = "default_search_params_order")]
    pub order: ResultOrder,
    /// Text to match against file contents, symbol names, and rendered signatures. Matching
    /// is case-insensitive and identifier-aware - the query and the fields split on case
    /// and underscore boundaries, so `loadConfig` finds `load_config`. Scoring is
    /// server-defined and comparable within one answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Files eligible for the search, selected by project-relative globs. Omitted selects
    /// every visible file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<PathSelector>,
    /// Extra payload to attach to every hit. Every entry costs response bytes per hit, so
    /// the caller requests only what it will read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<SearchInclude>>,
    /// Most hits to return in one page. `max_page_items` from the workspace resource caps
    /// it, and fewer may come back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1_u64, max = 10_000_u64))]
    pub limit: Option<u64>,
    /// Zero-based page of the result set to serve, sized by `limit`. A `page_index` past
    /// the last page returns an empty page whose `pagination` carries the requested
    /// `page_index` and the true `total_pages`.
    #[serde(default = "default_search_params_page_index")]
    pub page_index: u64,
    /// The version-control revision to search - a branch, tag, or commit id as the
    /// workspace's version control spells it. Omitted searches the current tree. The server
    /// refuses a revision search when the workspace has no version-control repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<RevisionId>,
    /// A bounded relationship walk, standing alone or beside `query`. A symbol the walk
    /// reaches becomes a hit tagged `relationship`; one also matched lexically keeps its
    /// lexical score and gains the walk's path. `target: "file"` never carries a walked hit,
    /// since a traversal only reaches symbols. With no `query`, `relevance` orders hits by
    /// ascending `distance`, then identity. Never combines with `rev`: the relationship
    /// graph serves the current tree alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traversal: Option<SearchTraversal>,
}

fn default_search_params_target() -> SearchParamsTarget {
    SearchParamsTarget::All
}

fn default_search_params_order() -> ResultOrder {
    ResultOrder::Relevance
}

fn default_search_params_page_index() -> u64 {
    PAGE_INDEX_DEFAULT
}

/// Which entity kinds may be returned.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SearchParamsTarget {
    /// Only declarations may match.
    Symbol,
    /// Only tree entries may match.
    File,
    /// Any entity kind may match.
    All,
}

/// One page of search hits from one captured tree.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::declare_search_result_empty_defaults)]
#[schemars(extend("examples" = [
    {
        "results": [
            {
                "hit": {
                    "target": "symbol",
                    "symbol": {
                        "id": "rift://symbol/rust/src/config.rs/load_config",
                        "language": "rust",
                        "name": "load_config",
                        "kind": "function",
                        "facets": [
                            "value",
                            "callable",
                            "public"
                        ],
                        "visibility": "pub",
                        "types": [
                            {
                                "role": "return",
                                "origin": "declared",
                                "type": {
                                    "language": "rust",
                                    "source": "Result<Config, ConfigError>"
                                }
                            }
                        ],
                        "signatures": [
                            {
                                "display": "pub fn load_config(path: &Path) -> Result<Config, ConfigError>",
                                "links": [
                                    {
                                        "range": {
                                            "start": 42,
                                            "end": 48
                                        },
                                        "symbol": "rift://symbol/rust/src/config.rs/Config"
                                    }
                                ],
                                "language": "rust",
                                "parameters": [
                                    {
                                        "name": "path",
                                        "types": [
                                            {
                                                "role": "parameter",
                                                "origin": "declared",
                                                "type": {
                                                    "language": "rust",
                                                    "source": "&Path"
                                                }
                                            }
                                        ],
                                        "optional": false,
                                        "variadic": false
                                    }
                                ],
                                "returns": [
                                    {
                                        "role": "return",
                                        "origin": "declared",
                                        "type": {
                                            "language": "rust",
                                            "source": "Result<Config, ConfigError>"
                                        }
                                    }
                                ]
                            }
                        ],
                        "documentation": [
                            {
                                "format": "markdown",
                                "text": "Loads the workspace configuration from `rift.toml`."
                            }
                        ]
                    }
                },
                "matched_by": [
                    "name"
                ],
                "source": "/// Loads the workspace configuration from `rift.toml`.\npub fn load_config(path: &Path) -> Result<Config, ConfigError> {\n    let text = std::fs::read_to_string(path)?;\n    parse_config(&text)\n}",
                "range": {
                    "start": 162,
                    "end": 355
                },
                "line": 10,
                "path": "src/config.rs"
            },
            {
                "hit": {
                    "target": "file",
                    "size": 241,
                    "languages": [
                        "rust"
                    ]
                },
                "matched_by": [
                    "content"
                ],
                "source": "    let config = load_config(&arguments.path)?;",
                "range": {
                    "start": 121,
                    "end": 168
                },
                "line": 7,
                "path": "src/lib.rs"
            }
        ],
        "pagination": {
            "page_index": 0,
            "total_pages": 3
        }
    }
]))]
pub struct SearchResult {
    /// The hits on this page, in the order the request asked for.
    pub results: Vec<SearchHit>,
    /// Where this page sits in the full result set under the request's `limit`.
    pub pagination: Pagination,
    /// Warnings attached to this result. Absent when there is nothing to warn about.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<ReadWarning>,
}

/// Default `depth` for a search traversal: one hop, because a second hop can multiply weak
/// edges.
pub const SEARCH_TRAVERSAL_DEPTH_DEFAULT: u64 = 1;
/// Least `depth` a search traversal accepts.
pub const SEARCH_TRAVERSAL_DEPTH_MIN: u64 = 1;
/// Most `depth` a search traversal accepts - the same bound `SearchHit.traversal_path`'s
/// length and `SearchHit.distance`'s range carry, since a hop count and a path length name
/// the same walk.
pub const SEARCH_TRAVERSAL_DEPTH_MAX: u64 = 2;
/// Most facets one `SearchTraversal.facets` list may carry: `RelationshipFacet`'s own variant
/// count, so a list padded past every distinct facet is refused rather than accepted and
/// silently deduplicated.
pub const SEARCH_TRAVERSAL_FACETS_MAX: usize = RelationshipFacet::VARIANTS.len();

/// A bounded relationship walk starting at one symbol. From `seed`, the server visits its
/// neighbors, then their neighbors, up to `depth` hops, following `direction` and narrowed to
/// `facets`; each reached symbol keeps the shortest path the walk found to it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchTraversal {
    /// The declaration the walk starts at. The seed itself is never a hit.
    pub seed: SymbolId,
    /// Which edges the walk follows from each visited symbol. Omitted, `outgoing`.
    #[serde(default = "default_search_traversal_direction")]
    pub direction: TraversalDirection,
    /// Portable relationship facets eligible for the walk. Omitted or empty, every facet is
    /// eligible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 29))]
    pub facets: Vec<RelationshipFacet>,
    /// Hops the walk may take from `seed`. The server accepts 1 or 2. Omitted, 1.
    #[serde(default = "default_search_traversal_depth")]
    #[schemars(range(min = 1_u64, max = 2_u64))]
    pub depth: u64,
    /// When set, the answer keeps only the hit whose walk reaches this symbol, at its
    /// shortest path. A `to` the walk cannot reach within `depth` answers empty, not refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<SymbolId>,
}

fn default_search_traversal_direction() -> TraversalDirection {
    TraversalDirection::Outgoing
}

fn default_search_traversal_depth() -> u64 {
    SEARCH_TRAVERSAL_DEPTH_DEFAULT
}

/// Which edge direction a search traversal walks from each visited symbol.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TraversalDirection {
    /// Walks edges leaving each visited symbol.
    Outgoing,
    /// Walks edges arriving at each visited symbol.
    Incoming,
    /// Walks edges in both directions.
    Both,
}

#[cfg(test)]
mod tests {
    use super::{
        PAGE_INDEX_DEFAULT, PathPattern, PathPatternViolation, SEARCH_TRAVERSAL_DEPTH_DEFAULT,
        SEARCH_TRAVERSAL_DEPTH_MAX, SEARCH_TRAVERSAL_DEPTH_MIN, SEARCH_TRAVERSAL_FACETS_MAX,
        SearchHit, SearchParams, SearchTraversal, TraversalDirection,
    };
    use serde_json::json;

    /// Attribute arguments and `#[serde(default = ...)]` functions are both compiled apart
    /// from the schema; this pins the advertised default to the constant the field's
    /// default function returns.
    #[test]
    fn search_params_schema_page_index_default_equals_the_enforced_constant() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchParams)).expect("schema");
        assert_eq!(
            schema["properties"]["page_index"]["default"],
            json!(PAGE_INDEX_DEFAULT)
        );
    }

    /// `include: ["body"]` names no `SearchInclude` member; a request naming it is
    /// refused at deserialization, and the refusal names the accepted values.
    #[test]
    fn search_include_rejects_an_unknown_entry_and_names_the_accepted_values() {
        let error =
            serde_json::from_value::<SearchParams>(json!({"query": "Beacon", "include": ["body"]}))
                .expect_err("an unknown include entry must fail deserialization");
        let message = error.to_string();
        assert!(
            message.contains("source") && message.contains("score"),
            "{message}"
        );
    }

    #[test]
    fn path_pattern_violation_classifies_every_schema_rule() {
        let cases = [
            ("", Some(PathPatternViolation::Empty)),
            ("/src/lib.rs", Some(PathPatternViolation::Absolute)),
            ("src\\lib.rs", Some(PathPatternViolation::Backslash)),
            ("dir/back\\slash.rs", Some(PathPatternViolation::Backslash)),
            (
                "src/line\n.rs",
                Some(PathPatternViolation::ControlCharacter),
            ),
            ("../outside.rs", Some(PathPatternViolation::DotSegment)),
            ("src/../lib.rs", Some(PathPatternViolation::DotSegment)),
            ("src/**/*.rs", None),
            ("README.md", None),
        ];
        for (value, expected) in cases {
            assert_eq!(
                PathPattern(value.to_owned()).violation(),
                expected,
                "value={value:?}"
            );
        }
    }

    #[test]
    fn path_pattern_violation_as_str_matches_serde_spelling() {
        let violations = [
            PathPatternViolation::Empty,
            PathPatternViolation::Absolute,
            PathPatternViolation::Backslash,
            PathPatternViolation::ControlCharacter,
            PathPatternViolation::DotSegment,
        ];
        for violation in violations {
            assert_eq!(
                serde_json::to_value(violation).ok(),
                Some(json!(violation.as_str())),
                "violation={violation:?}"
            );
        }
    }

    /// Attribute arguments and `#[serde(default = ...)]` functions are both compiled apart
    /// from the schema; this pins the advertised defaults to the constants their default
    /// functions return.
    #[test]
    fn search_traversal_schema_defaults_equal_the_enforced_constants() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchTraversal)).expect("schema");
        let properties = &schema["properties"];
        assert_eq!(properties["direction"]["default"], json!("outgoing"));
        assert_eq!(
            properties["depth"]["default"],
            json!(SEARCH_TRAVERSAL_DEPTH_DEFAULT)
        );
    }

    /// `#[schemars(length(max = ...))]` and `#[schemars(range(min = ..., max = ...))]` take
    /// only literals, so this pins every literal this PR added back to the named constants
    /// that state what they mean - a future facet or a hand-edited literal fails here first.
    #[test]
    fn search_traversal_schema_bounds_equal_the_enforced_constants() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchTraversal)).expect("schema");
        let properties = &schema["properties"];
        assert_eq!(
            properties["facets"]["maxItems"],
            json!(SEARCH_TRAVERSAL_FACETS_MAX)
        );
        assert_eq!(
            properties["depth"]["minimum"],
            json!(SEARCH_TRAVERSAL_DEPTH_MIN)
        );
        assert_eq!(
            properties["depth"]["maximum"],
            json!(SEARCH_TRAVERSAL_DEPTH_MAX)
        );
    }

    /// A hop count and a path length name the same walk: `SearchTraversal.depth`,
    /// `SearchHit.traversal_path`'s length, and `SearchHit.distance`'s range must all agree,
    /// or a depth this schema accepts could mint a hit its own schema refuses.
    #[test]
    fn search_traversal_depth_bound_matches_the_shipped_hit_bounds() {
        let hit_schema = serde_json::to_value(schemars::schema_for!(SearchHit)).expect("schema");
        let hit_properties = &hit_schema["properties"];
        assert_eq!(
            hit_properties["traversal_path"]["minItems"],
            json!(SEARCH_TRAVERSAL_DEPTH_MIN)
        );
        assert_eq!(
            hit_properties["traversal_path"]["maxItems"],
            json!(SEARCH_TRAVERSAL_DEPTH_MAX)
        );
        assert_eq!(
            hit_properties["distance"]["minimum"],
            json!(SEARCH_TRAVERSAL_DEPTH_MIN)
        );
        assert_eq!(
            hit_properties["distance"]["maximum"],
            json!(SEARCH_TRAVERSAL_DEPTH_MAX)
        );
    }

    /// `deny_unknown_fields` refuses a request naming a field this model never served, such
    /// as the withdrawn `intent` or `max_hops` PR #171 removed.
    #[test]
    fn search_traversal_rejects_an_unknown_field() {
        let result: Result<SearchTraversal, _> = serde_json::from_value(json!({
            "seed": "rift://symbol/rust/src/lib.rs/beacon",
            "intent": "trace"
        }));
        assert!(
            result.is_err(),
            "an unknown traversal field must fail deserialization"
        );
    }

    #[test]
    fn search_params_with_traversal_and_no_query_parses() {
        let params: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/src/lib.rs/beacon"
            }
        }))
        .expect("a traversal-only request must parse");
        let traversal = params.traversal.expect("traversal must be present");
        assert_eq!(traversal.direction, TraversalDirection::Outgoing);
        assert_eq!(traversal.depth, SEARCH_TRAVERSAL_DEPTH_DEFAULT);
        assert!(traversal.facets.is_empty());
        assert!(traversal.to.is_none());
    }
}
