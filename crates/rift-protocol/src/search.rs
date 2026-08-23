//! Wire models for the `search` MCP tool: request criteria, the filter tree, traversal, and
//! result hits. Extracted from [`crate::read`] so that module stays below its size bound; every
//! type here is re-exported from `read` so existing `rift_protocol::read::SearchParams`-style
//! paths keep resolving.

use crate::read::{
    Coverage, Cursor, DiagnosticContext, File, Node, ProjectPath, ProjectionId, ReadSnapshot,
    Relationship, RelationshipFacet, RevisionId, SearchScope, SourceUnitSpan, Symbol, SymbolId,
};
use crate::schema;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A predicate over one entry of a list-valued field. `Symbol.types` holds several entries,
/// and a filter that tests `role` and the resolved symbol separately would accept a symbol
/// whose return type and whose `Config` came from two different entries. Everything under
/// `where` addresses one entry, so both have to hold of the same one.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ElementFilter {
    /// The list-valued field to walk, by its name in this model: `types`, `signatures`.
    pub field: String,
    /// What one entry has to satisfy. Field names inside address the entry, not the entity
    /// that holds it.
    pub r#where: Box<Filter>,
}

/// A predicate over a standard, namespaced substrate, or diagnostic field. Rift evaluates
/// the regex operation under `rift-regex`. Path selectors carry their own glob grammar.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FieldFilter {
    /// Which field to test, by its name in this model: `facets`, `origin.location.kind`,
    /// `origin.source_kind`, `severity`. Extension keys and diagnostic fields are addressed
    /// the same way.
    pub field: String,
    /// How the operand is compared against the field. What a comparison means follows the
    /// field's type, so ordering ops apply only where the values are ordered. An array field
    /// such as `facets` takes `contains`, `in` and `exists`; the rest have no meaning
    /// against a list and Rift rejects them.
    pub op: FieldFilterOp,
    /// The operand, for every op except `in` and `exists`.
    #[serde(default)]
    pub value: Option<serde_json::Value>,
    /// The operands for `in`.
    #[serde(default)]
    pub values: Option<Vec<serde_json::Value>>,
}

/// How the operand is compared against the field. What a comparison means follows the
/// field's type, so ordering ops apply only where the values are ordered.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum FieldFilterOp {
    /// The field equals the value.
    Eq,
    /// The field differs from the value.
    Ne,
    /// The field equals one of the listed values.
    In,
    /// The array field holds the value as an entry.
    Contains,
    /// The field starts with the value.
    Prefix,
    /// The field matches the value as a regular expression.
    Regex,
    /// The field is greater than the value.
    Gt,
    /// The field is greater than or equal to the value.
    Gte,
    /// The field is less than the value.
    Lt,
    /// The field is less than or equal to the value.
    Lte,
    /// The field is present, whatever its value.
    Exists,
}

/// A recursive typed predicate. Every branch is tagged, so a filter tree parses in one pass.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", deny_unknown_fields, rename_all = "snake_case")]
pub enum Filter {
    /// A test on one field of the entity.
    Field {
        /// The field and the comparison.
        field: FieldFilter,
    },
    /// A test on the edges the entity has.
    Relation {
        /// The edges to look for, and what they must reach.
        relation: Box<RelationFilter>,
    },
    /// Conjunction: every member has to hold.
    All {
        /// The filters that must all hold.
        all: Vec<Filter>,
    },
    /// Disjunction: at least one member has to hold.
    Any {
        /// The filters, of which one is enough.
        any: Vec<Filter>,
    },
    /// Negation of what it holds.
    Not {
        /// The filter being negated.
        not: Box<Filter>,
    },
    /// A test on one entry of a list the entity holds.
    Element {
        /// The list to walk, and what one of its entries has to satisfy.
        element: Box<ElementFilter>,
    },
}

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
fn path_pattern_violation(value: &str) -> Option<PathPatternViolation> {
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

/// A predicate over an exact advertised relationship kind or portable facet.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::require_kind_or_facet)]
pub struct RelationFilter {
    /// Exact relationship kinds a provider emits. Any listed kind matches.
    #[serde(default)]
    pub kind: Option<Vec<String>>,
    /// Portable relationship facets. Any listed facet matches.
    #[serde(default)]
    pub facet: Option<Vec<RelationshipFacet>>,
    /// Which way the edge runs, seen from the entity being filtered.
    pub direction: RelationFilterDirection,
    /// What has to be true of the entity at the other end. Nesting a filter here is how
    /// "callers that are tests" becomes one query.
    #[serde(default)]
    pub target: Option<Box<Filter>>,
    /// How many edges to walk before a hit counts. Above 1 this asks about indirect
    /// neighbours and skips the direct ones.
    #[serde(default)]
    #[schemars(range(min = 1_u64))]
    pub min_depth: Option<u64>,
    /// How many edges a traversal may cross. Only edges that compose carry a depth -
    /// `contains`, `declares`, `augments`, `calls`, `imports`, `extends`, `implements`,
    /// `mixes_in`, `embeds`, `depends_on`. A bound above 1 on any other facet has nothing
    /// to walk, and Rift rejects it.
    #[serde(default)]
    #[schemars(range(min = 1_u64, max = 100_u64))]
    pub max_depth: Option<u64>,
    /// Whether a match needs such an edge, or needs there to be none.
    #[serde(default)]
    pub quantifier: Option<RelationFilterQuantifier>,
}

/// Which way the edge runs, seen from the entity being filtered.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RelationFilterDirection {
    /// The edge starts at the entity being filtered.
    Outgoing,
    /// The edge points at the entity being filtered.
    Incoming,
    /// The edge runs either way.
    Either,
}

/// Whether a match needs such an edge, or needs there to be none.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RelationFilterQuantifier {
    /// At least one such edge must exist.
    Exists,
    /// No such edge may exist.
    NotExists,
}

/// The total order a paginated answer comes back in, named in the request so a cursor can be
/// bound to it. Every order ends in the result's own identity, so two results that tie never
/// swap places between pages.
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
    /// How well this hit matched. Scores are comparable across every page of one request
    /// and nowhere else.
    pub score: f64,
    /// Which indexed fields produced the match.
    pub matched_by: Vec<MatchedField>,
    /// Edges from this hit, requested with `include: ["relationships"]`.
    #[serde(default)]
    pub relationships: Option<Vec<Relationship>>,
    /// The source text around the hit, requested with `include: ["source"]`. Covers the
    /// hit's `span`; a caller that needs the range already has it there.
    #[serde(default)]
    pub source: Option<String>,
    /// What providers reported here, requested with `include: ["diagnostics"]`.
    #[serde(default)]
    pub diagnostics: Option<Vec<DiagnosticContext>>,
    /// Where the hit is written in the source catalog. Null for a symbol whose source is
    /// unavailable or synthetic.
    #[serde(default)]
    pub span: Option<SourceUnitSpan>,
    /// The 1-based source line where the hit begins, or null with `span`.
    #[serde(default)]
    #[schemars(range(min = 1_u64))]
    pub line: Option<u64>,
    /// Project-relative path of the hit, where the location is a project path. Null for a
    /// hit whose only location is a dependency or standard-library source unit.
    #[serde(default)]
    pub path: Option<ProjectPath>,
    /// Shortest relationship path from `traversal.seed` to this hit. Present whenever the
    /// traversal reached the hit, including a hit also matched lexically.
    #[serde(default)]
    #[schemars(length(min = 1, max = 2))]
    pub traversal_path: Option<Vec<GraphHop>>,
    /// Number of edges in `traversal_path`. It is present exactly when `traversal_path` is
    /// present and equals its length.
    #[serde(default)]
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
    /// Rendered signatures for symbol hits.
    Signature,
    /// Edges from each hit.
    Relationships,
    /// Provider findings at each hit.
    Diagnostics,
}

/// The task that selects graph defaults and ranking. The server still returns each traversed
/// edge, so the caller can audit the ranking.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SearchIntent {
    /// Follows execution outward from the seed.
    Trace,
    /// Finds the tests that exercise the seed.
    FindTests,
    /// Estimates what an edit to the seed would disturb.
    EditRipple,
    /// Gathers what a reviewer of the seed should see.
    ReviewContext,
}

/// Criteria for one search. The caller supplies at least one of lexical `query`, provider
/// `filter`, or relationship `traversal`; `scope` selects source locations, and `paths`
/// narrows project-only searches.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(transform = schema::restrict_traversal_and_paths)]
#[schemars(transform = schema::require_query_filter_or_traversal)]
#[schemars(transform = schema::forbid_search_rev_with_projection)]
pub struct SearchParams {
    /// Which entity kinds may be returned - a kind selector, never the text to search for;
    /// that is `query`. Omitted, every kind may match. Type data is attached to the Symbol
    /// and Node records that bind it, and filters can search those attachments.
    #[serde(default = "default_search_params_target")]
    pub target: SearchParamsTarget,
    /// Which total order the page comes back in. The cursor is bound to it, so it cannot
    /// change between pages of one query. Omitted, relevance.
    #[serde(default = "default_search_params_order")]
    pub order: ResultOrder,
    /// Text to match against file contents, symbol names, and rendered signatures. Matching
    /// is case-insensitive and identifier-aware - the query and the fields split on case
    /// and underscore boundaries, so `loadConfig` finds `load_config`. Scoring is
    /// server-defined and stable for one cursor's life.
    #[serde(default)]
    pub query: Option<String>,
    /// A predicate over resolved fields and relationships. This is where provider knowledge
    /// enters a search - implements this trait, called by that function, declared under
    /// `src/api`.
    #[serde(default)]
    pub filter: Option<Filter>,
    /// Files eligible for the search, selected by project-relative globs. Omitted selects
    /// every visible file.
    #[serde(default)]
    pub paths: Option<PathSelector>,
    /// Extra payload to attach to every hit. Each entry costs a lookup per hit, so the
    /// caller requests only what it will read.
    #[serde(default)]
    pub include: Option<Vec<SearchInclude>>,
    /// Most hits to return in one page. `max_page_items` from the workspace resource caps
    /// it, and fewer may come back.
    #[serde(default)]
    #[schemars(range(min = 1_u64, max = 10_000_u64))]
    pub limit: Option<u64>,
    /// Continues a previous search where its last page ended. Omit it for the first page;
    /// everything else in the request has to match what the cursor was minted for.
    #[serde(default)]
    pub cursor: Option<Cursor>,
    /// The projection to search. Null searches the workspace tree.
    #[serde(default)]
    pub projection: Option<ProjectionId>,
    /// The version-control revision to search - a branch, tag, or commit id as the
    /// workspace's version control spells it. Null searches the current tree, and `rev`
    /// never combines with `projection`. The server refuses a revision search when the
    /// workspace has no version-control repository.
    #[serde(default)]
    pub rev: Option<RevisionId>,
    /// A bounded relationship walk. It may stand alone or add graph hits to a lexical or
    /// filtered search; duplicate symbols keep their shortest path.
    #[serde(default)]
    pub traversal: Option<SearchTraversal>,
    /// Source locations eligible for results. Project is the default; select dependencies
    /// or all when the answer may live outside the workspace.
    #[serde(default = "default_search_params_scope")]
    pub scope: SearchScope,
}

fn default_search_params_target() -> SearchParamsTarget {
    SearchParamsTarget::All
}

fn default_search_params_order() -> ResultOrder {
    ResultOrder::Relevance
}

fn default_search_params_scope() -> SearchScope {
    SearchScope::Project
}

/// Which entity kinds may be returned. Type data is attached to the Symbol and Node records
/// that bind it, and filters can search those attachments.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SearchParamsTarget {
    /// Only declarations may match.
    Symbol,
    /// Only syntax-tree nodes may match.
    Node,
    /// Only tree entries may match.
    File,
    /// Any entity kind may match.
    All,
}

/// One page of search hits from one captured tree and index revision. `coverage` states
/// whether an empty result proves that no indexed candidate matched.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchResult {
    /// Tree and search-index revisions used for this result page.
    pub snapshot: ReadSnapshot,
    /// Coverage of the indexed candidate set used for this search. An empty result proves
    /// no match only where this is complete.
    pub coverage: Coverage,
    /// The hits on this page, in the order the request asked for.
    pub results: Vec<SearchHit>,
    /// Cursor for the next page, or null after the final result.
    #[serde(deserialize_with = "crate::read::deserialize_required_option")]
    #[schemars(required, transform = schema::nullable)]
    pub next_cursor: Option<Cursor>,
}

/// Default `max_hops` for a search traversal: one hop, because a second hop can multiply weak
/// edges.
pub const SEARCH_TRAVERSAL_HOPS_DEFAULT: u64 = 1;
/// Default `max_nodes` for a search traversal.
pub const SEARCH_TRAVERSAL_NODES_DEFAULT: u64 = 25;

/// A bounded relationship walk starting at one symbol. The server visits at most
/// `max_nodes` symbols outside `seed` and expands no path beyond `max_hops`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchTraversal {
    /// The symbol where the walk starts. The seed is not returned as a hit.
    pub seed: SymbolId,
    /// The task whose defaults rank and narrow the walk. `find_tests` returns test symbols;
    /// other intents return every eligible symbol.
    pub intent: SearchIntent,
    /// Direction to walk. Omitted selects incoming for `find_tests` and `edit_ripple`,
    /// outgoing for `trace`, and both for `review_context`.
    #[serde(default)]
    pub direction: Option<TraversalDirection>,
    /// Portable relationship facets eligible for expansion. Omitted selects `tests` for
    /// `find_tests` and `calls` for every other intent; an empty list is `invalid_request`.
    #[serde(default)]
    #[schemars(length(min = 1))]
    pub facets: Option<Vec<RelationshipFacet>>,
    /// Maximum path length from `seed`. The server accepts 1 or 2; one hop is the default
    /// because a second hop can multiply weak edges.
    #[serde(default = "default_search_traversal_max_hops")]
    #[schemars(range(min = 1_u64, max = 2_u64))]
    pub max_hops: u64,
    /// Most distinct graph symbols the server may visit, excluding `seed`. Filtering can
    /// make the answer shorter than this bound.
    #[serde(default = "default_search_traversal_max_nodes")]
    #[schemars(range(min = 1_u64, max = 100_u64))]
    pub max_nodes: u64,
}

fn default_search_traversal_max_hops() -> u64 {
    SEARCH_TRAVERSAL_HOPS_DEFAULT
}

fn default_search_traversal_max_nodes() -> u64 {
    SEARCH_TRAVERSAL_NODES_DEFAULT
}

/// Which edge direction the server walks from each visited symbol.
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
        PathPattern, PathPatternViolation, SEARCH_TRAVERSAL_HOPS_DEFAULT,
        SEARCH_TRAVERSAL_NODES_DEFAULT, SearchTraversal,
    };
    use serde_json::json;

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
    /// from the schema; this pins the advertised default to the constant the field's default
    /// function returns.
    #[test]
    fn search_traversal_schema_defaults_equal_the_enforced_constants() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchTraversal)).expect("schema");
        let properties = &schema["properties"];
        assert_eq!(
            properties["max_hops"]["default"],
            json!(SEARCH_TRAVERSAL_HOPS_DEFAULT)
        );
        assert_eq!(
            properties["max_nodes"]["default"],
            json!(SEARCH_TRAVERSAL_NODES_DEFAULT)
        );
    }
}
