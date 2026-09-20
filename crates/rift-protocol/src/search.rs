//! Wire models for the `search` MCP tool: request criteria and result hits. Extracted from
//! [`crate::read`] so that module stays below its size bound; every type here is re-exported
//! from `read` so existing `rift_protocol::read::SearchParams`-style paths keep resolving.

use crate::read::{
    Language, NodeId, PAGE_INDEX_DEFAULT, PAGE_LIMIT_MAX, Pagination, ProjectPath, ReadWarning,
    Relationship, RelationshipFacet, RevisionId, SearchScope, SourceUnitId, Symbol, SymbolId,
    SymbolVersionKind, TextRange,
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
    /// A comparison of two committed revisions found the declaration changed.
    Change,
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

/// The field path the server names when a search's `paths.force_include` matches more
/// files than one request may pull into the index.
pub const FORCE_INCLUDE_FIELD: &str = "paths.force_include";

/// Which files a query runs over, as three lists of globs matched against the project-relative
/// path. The same glob engine backs the workspace's `[source]` policy. `include: ["src/**"]`
/// selects the source tree; `exclude: ["src/generated/**"]` then removes generated output.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [
    {
        "include": ["src/**"],
        "exclude": ["src/generated/**"]
    }
]))]
pub struct PathSelector {
    /// Globs a path has to match to be searched at all. Empty includes every visible file.
    #[serde(default)]
    pub include: Vec<PathPattern>,
    /// Globs that drop a path `include` already matched.
    #[serde(default)]
    pub exclude: Vec<PathPattern>,
    /// Globs reaching files the workspace's `[source]` policy or `.gitignore` excluded from
    /// the index. Matches are bounded per request - the bound counts the files the request
    /// reaches outside the index, so a glob whose matches the index already holds adds none -
    /// and the server refuses the search when the bound is crossed rather than truncating it
    /// silently.
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
    /// Byte range of the hit within `path` or `unit`. Absent for a symbol whose source is
    /// unavailable or synthetic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<TextRange>,
    /// The 1-based source line where the hit begins, or absent with `range`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1_u64))]
    pub line: Option<u64>,
    /// Project-relative path of the hit, present for a file hit and for a symbol hit whose
    /// declaration belongs to the project. A dependency or standard-library declaration
    /// carries `unit` in its place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<ProjectPath>,
    /// Source-catalog unit of a dependency or standard-library declaration. A symbol hit
    /// carries exactly one of `path` and `unit`; a file hit carries `path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<SourceUnitId>,
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
    /// How the declaration differs between the two revisions `change` named, present when
    /// the comparison produced this hit. The hit's `hit.symbol`, `path`, `range`, `line`,
    /// and `source` read the head revision, except for a removed declaration, which keeps
    /// its base-side identity, path, range, and source - the head revision no longer holds
    /// it. A change hit carries no `score`, since a comparison ranks nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<SymbolChange>,
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
        /// The syntax-tree node's identity in the captured source revision.
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

/// The field path the server names when a comparison's base revision spelling breaks the
/// contract [`RevisionId`] advertises.
pub const CHANGE_BASE_FIELD: &str = "change.base";

/// The field path the server names when a comparison's head revision spelling breaks that
/// same contract.
pub const CHANGE_HEAD_FIELD: &str = "change.head";

/// Default `head` for a change comparison: the revision the workspace's version control
/// currently has checked out.
pub const SEARCH_CHANGE_HEAD_DEFAULT: &str = "HEAD";

/// Most changed paths one comparison reads. A comparison that reaches it answers from the
/// paths that fit and warns `change_truncated`.
pub const SEARCH_CHANGE_PATHS_MAX: u64 = 512;

/// Two committed revisions to compare. The answer is the declarations the two revisions
/// hold differently: `base` is the revision compared from, `head` the revision compared
/// to. Both sides name a commit; uncommitted working-tree bytes take part in no
/// comparison.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [
    {
        "base": "main",
        "head": "HEAD"
    }
]))]
pub struct SearchChange {
    /// The revision compared from - a branch, tag, or commit id as the workspace's version
    /// control spells it.
    pub base: RevisionId,
    /// The revision compared to, spelled the same way. Omitted, `HEAD`.
    #[serde(default = "default_search_change_head")]
    pub head: RevisionId,
}

fn default_search_change_head() -> RevisionId {
    RevisionId(SEARCH_CHANGE_HEAD_DEFAULT.to_owned())
}

/// How one declaration differs between the two compared revisions, and where it lives on
/// each side.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolChange {
    /// What the head revision did to the declaration.
    pub kind: SymbolVersionKind,
    /// Where the declaration lived at the base revision. Absent for a declaration the head
    /// revision introduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_path: Option<ProjectPath>,
    /// Where the declaration lives at the head revision. Absent for a declaration the head
    /// revision removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_path: Option<ProjectPath>,
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
/// `traversal`, or both, or a `change` comparing two committed revisions; `paths` narrows
/// the files eligible for any of them.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::require_search_selector)]
#[schemars(transform = schema::require_traversal_seed)]
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
                "references"
            ]
        },
        "limit": 25
    },
    {
        "target": "symbol",
        "query": "spawn_blocking",
        "scope": "dependencies",
        "limit": 10
    },
    {
        "target": "symbol",
        "change": {
            "base": "main",
            "head": "HEAD"
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
    /// Which declarations `query` searches: the project tree, the public declarations of
    /// the cataloged dependency packages, or both. Omitted, `project`. A package
    /// contributes symbol hits alone. The server refuses a scope beyond `project`
    /// together with `rev`, since dependencies are served for the current tree alone,
    /// and `dependencies` together with `traversal`, since the relationship graph serves
    /// the project alone.
    #[serde(default)]
    pub scope: SearchScope,
    /// Files eligible for the search, selected by project-relative globs. Omitted selects
    /// every visible file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<PathSelector>,
    /// Extra payload to attach to every hit. Every entry costs response bytes per hit, so
    /// the caller requests only what it will read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<SearchInclude>>,
    /// Most hits to return in one page, at most 10,000; the server refuses a larger
    /// `limit` naming the field. The server's result bound caps the set itself, and an
    /// answer whose set reached it warns `results_truncated`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1_u64, max = PAGE_LIMIT_MAX))]
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
    /// A bounded relationship walk from `seed`, standing alone or beside `query`. A symbol
    /// the walk reaches becomes a hit tagged `relationship`; one also matched lexically
    /// keeps its own score and payload and gains the walk's path. `target: "file"` never
    /// carries a walked hit, since a traversal only reaches symbols. With no `query`,
    /// `relevance` orders hits by ascending `distance`, then identity. A configured
    /// language engine resolves the references the walk follows, and an engine session
    /// serves the current tree, so the server refuses `traversal` beside `rev` and beside
    /// `change`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traversal: Option<SearchTraversal>,
    /// Two committed revisions to compare, standing alone. Every declaration the two
    /// revisions hold differently becomes a hit tagged `change`, carrying a `change` block
    /// that names what differs and where the declaration lives on each side.
    /// `target: "file"` never carries a change hit, since the comparison reaches
    /// declarations alone, and `relevance` orders the hits by path, then name. `paths`
    /// narrows which changed paths are compared. The server refuses `change` beside `rev`,
    /// since `change` names its own revisions; beside `query`, since the two select
    /// different result sets; beside `traversal`, since no lane resolves references for a
    /// committed revision; and beside a `scope` past `project`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<SearchChange>,
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

/// A bounded relationship walk starting at one symbol.
///
/// From `seed`, the server visits its neighbors, then their neighbors, up to `depth` hops,
/// following `direction` and narrowed to `facets`; each reached symbol keeps the shortest
/// path the walk found to it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [
    {
        "seed": "rift://symbol/rust/crates/rift-server/src/read.rs/ReadService",
        "direction": "incoming",
        "depth": 1
    }
]))]
pub struct SearchTraversal {
    /// The declaration the walk starts at. The seed itself is never a hit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<SymbolId>,
    /// Which edges the walk follows from each visited symbol. Omitted, `incoming`.
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
    /// When set, the answer keeps only the hit whose walk reaches this symbol. That hit's
    /// path is the shortest one the walk found. A `to` the walk cannot reach within `depth`
    /// answers empty, not refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<SymbolId>,
}

fn default_search_traversal_direction() -> TraversalDirection {
    TraversalDirection::Incoming
}

fn default_search_traversal_depth() -> u64 {
    SEARCH_TRAVERSAL_DEPTH_DEFAULT
}

/// Which edge direction a search traversal walks from each visited symbol.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TraversalDirection {
    /// Walks edges arriving at each visited symbol.
    #[default]
    Incoming,
}

#[cfg(test)]
mod tests {
    use super::{
        PAGE_INDEX_DEFAULT, PAGE_LIMIT_MAX, PathPattern, PathPatternViolation,
        SEARCH_CHANGE_HEAD_DEFAULT, SEARCH_TRAVERSAL_DEPTH_DEFAULT, SEARCH_TRAVERSAL_DEPTH_MAX,
        SEARCH_TRAVERSAL_DEPTH_MIN, SEARCH_TRAVERSAL_FACETS_MAX, SearchChange, SearchHit,
        SearchParams, SearchScope, SearchTraversal, TraversalDirection,
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

    /// The schema's `maximum` on `limit` and `accepted_limit`'s refusal both read
    /// `PAGE_LIMIT_MAX`; this pins the advertised maximum to that one constant.
    #[test]
    fn search_params_schema_limit_maximum_equals_the_enforced_constant() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchParams)).expect("schema");
        assert_eq!(
            schema["properties"]["limit"]["maximum"],
            json!(PAGE_LIMIT_MAX)
        );
    }

    /// `scope` takes serde's `default`, which reads the enum's own `Default`; this pins
    /// the advertised default to the `project` member that impl selects, the default
    /// `get_symbol` advertises for the same field.
    #[test]
    fn search_params_schema_scope_default_is_project() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchParams)).expect("schema");
        assert_eq!(schema["properties"]["scope"]["default"], json!("project"));
        assert_eq!(
            serde_json::to_value(SearchScope::default()).expect("serialize"),
            json!("project")
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
        assert_eq!(properties["direction"]["default"], json!("incoming"));
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

    /// The schema states the same rule the server enforces: a request selects its
    /// result set with `query`, `traversal`, or `change`.
    #[test]
    fn search_params_schema_states_the_three_result_set_selectors() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchParams)).expect("schema");
        let selector = schema["allOf"]
            .as_array()
            .and_then(|clauses| clauses.iter().find(|clause| clause.get("anyOf").is_some()))
            .expect("the selector rule states an anyOf");
        let required: Vec<&str> = selector["anyOf"]
            .as_array()
            .expect("the rule lists its alternatives")
            .iter()
            .filter_map(|clause| clause["required"][0].as_str())
            .collect();
        assert_eq!(required, ["query", "traversal", "change"], "{schema:#}");
    }

    /// The schema states the same rule the server enforces: a walk names `seed` when it
    /// stands without `change`, and names none beside one.
    #[test]
    fn search_params_schema_states_where_a_walk_starts() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchParams)).expect("schema");
        let clauses = schema["allOf"]
            .as_array()
            .expect("the rules state an allOf");
        let seeded = clauses
            .iter()
            .find(|clause| clause["properties"]["traversal"]["required"] == json!(["seed"]))
            .expect("the schema requires a seed on every walk");
        assert_eq!(
            seeded["properties"]["traversal"]["required"],
            json!(["seed"])
        );
        let beside_change = clauses
            .iter()
            .find(|clause| {
                clause.get("then").is_some() && clause["if"]["required"] == json!(["change"])
            })
            .expect("the riding-beside rule states an if/then");
        assert_eq!(
            beside_change["then"]["not"]["required"],
            json!(["traversal"])
        );
    }

    /// `head` takes a `#[serde(default = ...)]` function compiled apart from the
    /// schema; this pins the advertised default to the constant that function returns.
    #[test]
    fn search_change_schema_head_default_equals_the_enforced_constant() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchChange)).expect("schema");
        assert_eq!(
            schema["properties"]["head"]["default"],
            json!(SEARCH_CHANGE_HEAD_DEFAULT)
        );
    }

    /// A `change` naming `base` alone compares it against `HEAD`.
    #[test]
    fn search_change_with_base_alone_compares_against_head() {
        let params: SearchParams =
            serde_json::from_value(json!({"change": {"base": "main"}})).expect("a change parses");
        let change = params.change.expect("change must be present");
        assert_eq!(change.base.0, "main");
        assert_eq!(change.head.0, SEARCH_CHANGE_HEAD_DEFAULT);
    }

    /// `deny_unknown_fields` refuses a comparison naming a side this model never
    /// served, such as an uncommitted working-tree side.
    #[test]
    fn search_change_rejects_an_unknown_field() {
        let result: Result<SearchChange, _> =
            serde_json::from_value(json!({"base": "main", "worktree": true}));
        assert!(
            result.is_err(),
            "an unknown comparison field must fail deserialization"
        );
    }

    /// `deny_unknown_fields` refuses a request naming a field this model never served, such
    /// as the withdrawn `intent` or `max_hops` fields.
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
        assert_eq!(traversal.direction, TraversalDirection::Incoming);
        assert_eq!(traversal.depth, SEARCH_TRAVERSAL_DEPTH_DEFAULT);
        assert!(traversal.facets.is_empty());
        assert!(traversal.to.is_none());
    }
}
