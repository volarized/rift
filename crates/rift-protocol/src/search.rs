//! Wire models for the `search` MCP tool: request criteria and result hits. Extracted from
//! [`crate::read`] so that module stays below its size bound; every type here is re-exported
//! from `read` so existing `rift_protocol::read::SearchParams`-style paths keep resolving.

use crate::read::{
    File, Node, PAGE_INDEX_DEFAULT, Pagination, ProjectPath, ReadWarning, Relationship, RevisionId,
    SourceUnitSpan, Symbol,
};
use crate::schema;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
#[schemars(transform = schema::pair_span_with_line)]
pub struct SearchHit {
    /// What was found. A symbol, a node, or a file - whichever `target` allowed.
    pub hit: SearchHitTarget,
    /// How well this hit matched. Scores are comparable within one answer and nowhere
    /// else.
    pub score: f64,
    /// Which indexed fields produced the match.
    pub matched_by: Vec<MatchedField>,
    /// The source text around the hit, requested with `include: ["source"]`. Covers the
    /// hit's `span`; a caller that needs the range already has it there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Where the hit is written in the source catalog. Absent for a symbol whose source
    /// is unavailable or synthetic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceUnitSpan>,
    /// The 1-based source line where the hit begins, or absent with `span`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1_u64))]
    pub line: Option<u64>,
    /// Project-relative path of the hit, where the location is a project path. Absent for
    /// a hit whose only location is a dependency or standard-library source unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<ProjectPath>,
    /// Shortest relationship path from `traversal.seed` to this hit. Present whenever the
    /// traversal reached the hit, including a hit also matched lexically.
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
pub enum SearchHitTarget {
    /// A symbol hit: the declaration a provider resolved.
    Symbol {
        /// The declaration that matched.
        symbol: Symbol,
    },
    /// A node hit: one place in a syntax tree, without its enclosing symbol record.
    Node {
        /// The syntax-tree node that matched, and the symbol written at it where there is
        /// one.
        node: Node,
    },
    /// A file hit: one entry of the tree, whether or not any provider reads it.
    File {
        /// The tree entry that matched: what it holds, and which languages read it.
        file: File,
    },
}

/// Extra payload to attach to every hit. Each entry costs a lookup per hit, so the caller
/// requests only what it will read.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SearchInclude {
    /// The source text around each hit.
    Source,
}

/// Criteria for one search. The caller supplies lexical `query`, and `paths` narrows the
/// files eligible for it.
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
    /// Extra payload to attach to every hit. Each entry costs a lookup per hit, so the
    /// caller requests only what it will read.
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
#[schemars(extend("examples" = [
    {
        "results": [
            {
                "hit": {
                    "target": "symbol",
                    "symbol": {
                        "id": "rift://symbol/rust/src/config.rs/load_config",
                        "language": {
                            "name": "rust"
                        },
                        "name": "load_config",
                        "kind": "rust.function",
                        "facets": [
                            "value",
                            "callable",
                            "public"
                        ],
                        "origin": {
                            "location": {
                                "kind": "project"
                            },
                            "source_kind": "authored",
                            "unit": "rift://source/project/src/config.rs"
                        },
                        "modifiers": [],
                        "visibility": "pub",
                        "types": [
                            {
                                "role": "return",
                                "origin": "declared",
                                "type": {
                                    "language": {
                                        "name": "rust"
                                    },
                                    "source": "Result<Config, ConfigError>",
                                    "extensions": {}
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
                                "language": {
                                    "name": "rust"
                                },
                                "parameters": [
                                    {
                                        "name": "path",
                                        "types": [
                                            {
                                                "role": "parameter",
                                                "origin": "declared",
                                                "type": {
                                                    "language": {
                                                        "name": "rust"
                                                    },
                                                    "source": "&Path",
                                                    "extensions": {}
                                                }
                                            }
                                        ],
                                        "optional": false,
                                        "variadic": false,
                                        "extensions": {}
                                    }
                                ],
                                "returns": [
                                    {
                                        "role": "return",
                                        "origin": "declared",
                                        "type": {
                                            "language": {
                                                "name": "rust"
                                            },
                                            "source": "Result<Config, ConfigError>",
                                            "extensions": {}
                                        }
                                    }
                                ],
                                "type_parameters": [],
                                "throws": [],
                                "effects": [],
                                "extensions": {}
                            }
                        ],
                        "documentation": [
                            {
                                "format": "markdown",
                                "text": "Loads the workspace configuration from `rift.toml`."
                            }
                        ],
                        "extensions": {},
                        "document_local": false
                    }
                },
                "score": 0.9,
                "matched_by": [
                    "name"
                ],
                "source": "/// Loads the workspace configuration from `rift.toml`.\npub fn load_config(path: &Path) -> Result<Config, ConfigError> {\n    let text = std::fs::read_to_string(path)?;\n    parse_config(&text)\n}",
                "span": {
                    "unit": "rift://source/project/src/config.rs",
                    "range": {
                        "start": 162,
                        "end": 355
                    }
                },
                "line": 10,
                "path": "src/config.rs"
            },
            {
                "hit": {
                    "target": "file",
                    "file": {
                        "id": "rift://file/src/lib.rs",
                        "content": {
                            "kind": "regular",
                            "size": 241,
                            "executable": false
                        },
                        "languages": [
                            {
                                "name": "rust"
                            }
                        ],
                        "regions": [],
                        "semantic": true
                    }
                },
                "score": 1.0,
                "matched_by": [
                    "content"
                ],
                "source": "    let config = load_config(&arguments.path)?;",
                "span": {
                    "unit": "rift://source/project/src/lib.rs",
                    "range": {
                        "start": 121,
                        "end": 168
                    }
                },
                "line": 7,
                "path": "src/lib.rs"
            }
        ],
        "pagination": {
            "page_index": 0,
            "total_pages": 3
        },
        "warnings": []
    }
]))]
pub struct SearchResult {
    /// The hits on this page, in the order the request asked for.
    pub results: Vec<SearchHit>,
    /// Where this page sits in the full result set under the request's `limit`.
    pub pagination: Pagination,
    /// Warnings attached to this result, empty when there is nothing to warn about.
    pub warnings: Vec<ReadWarning>,
}

#[cfg(test)]
mod tests {
    use super::{PAGE_INDEX_DEFAULT, PathPattern, PathPatternViolation, SearchParams};
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
}
