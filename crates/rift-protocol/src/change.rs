//! Wire models for the Rift MCP change tools.
//!
//! Every type here is a wire contract: serde attributes define exactly what
//! the server accepts and returns, and the MCP server derives its advertised
//! request and response schemas from these definitions.

use crate::configuration::GuaranteeKind;
use crate::read::{
    CoverageScope, Diagnostic, NodeId, ProjectPath, RegionRole, SourceSpan, SymbolId,
};
use crate::schema;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Identity of one applied change, minted when the change lands: the first eight
/// lowercase hex characters of the SHA-256 of the change's content, the same wire form
/// `Digest` uses.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ChangeId(
    #[schemars(example = &"d54ffb22")]
    #[schemars(regex(pattern = r"^[0-9a-f]{8}$"))]
    pub String,
);

/// Which side of the anchor or file target receives the new content.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
pub enum InsertPosition {
    /// Wire value `before`.
    #[serde(rename = "before")]
    Before,
    /// Wire value `after`.
    #[serde(rename = "after")]
    After,
}

/// Why resolution produced no edits at all. `ErrorData` carries transport and
/// infrastructure failures.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
pub enum RefusalReason {
    /// Nothing serves this operation for the language it reaches.
    #[serde(rename = "unsupported")]
    Unsupported,
    /// A condition checked before resolution failed. The failed entry is in
    /// `preconditions`.
    #[serde(rename = "unmet_precondition")]
    UnmetPrecondition,
}

/// A filesystem effect described before Rift performs it. Edits in one set share one input
/// state, cannot overlap, and apply atomically.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Edit {
    #[serde(rename = "replace")]
    /// One byte range of one file and what replaces it.
    Replace {
        /// The file, and the byte range being replaced.
        span: SourceSpan,
        /// What the range becomes. Empty deletes it.
        #[schemars(length(max = 1_048_576))]
        text: String,
    },
}

/// A typed value compared by an operation precondition.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum PreconditionValue {
    #[serde(rename = "boolean")]
    /// Boolean property such as target existence or writability.
    Boolean {
        /// The value.
        value: bool,
    },
    #[serde(rename = "count")]
    /// Non-negative count such as remaining usages.
    Count {
        /// The value.
        #[schemars(range(min = 0_u64, max = 9_007_199_254_740_991_u64))]
        value: u64,
    },
    #[serde(rename = "text")]
    /// Language or policy value whose spelling is itself significant.
    Text {
        /// The value.
        #[schemars(length(max = 4096))]
        value: String,
    },
}

/// Existing semantic or source subject a precondition addresses.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum PreconditionAddress {
    #[serde(rename = "symbol")]
    /// A declaration, addressed by its symbol.
    Symbol {
        /// The symbol addressed.
        symbol: SymbolId,
    },
    #[serde(rename = "node")]
    /// A syntax node, addressed by its witnessed identity.
    Node {
        /// The node addressed.
        node: NodeId,
    },
}

/// Condition being checked.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
pub enum OperationPreconditionKind {
    /// Wire value `target_exists`.
    #[serde(rename = "target_exists")]
    TargetExists,
    /// Wire value `source_unchanged`.
    #[serde(rename = "source_unchanged")]
    SourceUnchanged,
    /// Wire value `no_references`.
    #[serde(rename = "no_references")]
    NoReferences,
    /// Wire value `body_readable`. The failed entry names the [`BodySource`] `file`
    /// form: absent, not a plain file, unreadable, or holding bytes that are not
    /// valid UTF-8 text.
    #[serde(rename = "body_readable")]
    BodyReadable,
    /// Wire value `engine_proposed_edits`. The failed entry names an operation whose
    /// configured engine declined it or proposed no edit for the addressed subject.
    #[serde(rename = "engine_proposed_edits")]
    EngineProposedEdits,
    /// Wire value `target_is_file`. The failed entry names a path a write requires to
    /// be a file, that a directory occupies on disk.
    #[serde(rename = "target_is_file")]
    TargetIsFile,
}

/// Result of this check.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
pub enum OperationPreconditionStatus {
    /// Wire value `satisfied`.
    #[serde(rename = "satisfied")]
    Satisfied,
    /// Wire value `failed`.
    #[serde(rename = "failed")]
    Failed,
}

/// One executable condition checked while resolving an operation.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperationPrecondition {
    /// The condition that was checked.
    pub kind: OperationPreconditionKind,
    /// Whether the condition held.
    pub status: OperationPreconditionStatus,
    /// Existing semantic or source subjects involved in the condition.
    #[schemars(length(max = 16))]
    pub addresses: Vec<PreconditionAddress>,
    /// Project paths involved in the condition, including destinations that do not yet
    /// exist.
    #[schemars(length(max = 64))]
    pub paths: Vec<ProjectPath>,
    /// Required value for this check.
    pub expected: PreconditionValue,
    /// Value found while checking the condition.
    pub observed: PreconditionValue,
}

impl OperationPrecondition {
    /// Builds one checked-condition record, wrapping plain path strings in
    /// the wire [`ProjectPath`].
    #[must_use]
    pub fn new(
        kind: OperationPreconditionKind,
        status: OperationPreconditionStatus,
        addresses: Vec<PreconditionAddress>,
        paths: Vec<String>,
        expected: PreconditionValue,
        observed: PreconditionValue,
    ) -> Self {
        Self {
            kind,
            status,
            addresses,
            paths: paths.into_iter().map(ProjectPath).collect(),
            expected,
            observed,
        }
    }
}

/// A property a passing hook established about one applied change. The
/// server mints one entry per configured guarantee of each hook that
/// passed, so a green run carries what it actually proved rather than an
/// unexplained tick.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuaranteeEvidence {
    /// Which property is established.
    pub kind: GuaranteeKind,
    /// What the check covered.
    pub scope: CoverageScope,
    /// The configured hook whose passing run established the property.
    #[schemars(length(min = 1, max = 64))]
    pub hook: String,
    /// The exact property checked and the limits on reading a pass, from
    /// the hook's configuration.
    #[schemars(length(min = 1, max = 1_024))]
    pub detail: String,
}

/// One applied change, with everything Rift learned while resolving it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeSummary {
    /// Identity of this applied change.
    pub id: ChangeId,
    /// Paths whose entries differ because of this change, sorted bytewise.
    #[schemars(length(min = 1, max = 256))]
    pub paths: Vec<ProjectPath>,
    /// Concrete edits in canonical file-and-range order. A modification carries one
    /// edit per replaced range; a file the change created or removed carries one edit
    /// spanning the whole file.
    #[schemars(length(min = 1, max = 256))]
    pub edits: Vec<Edit>,
    /// Resolution findings in source order, then one finding per hook that
    /// did not pass.
    #[schemars(length(max = 256))]
    pub diagnostics: Vec<Diagnostic>,
    /// Properties the workspace's passing validation hooks established about this
    /// change, in hook list order.
    #[schemars(length(max = 512))]
    pub guarantees: Vec<GuaranteeEvidence>,
}

/// An applied change or semantic refusal. For every refusal the targeted tree is
/// unchanged.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", deny_unknown_fields)]
#[schemars(extend("examples" = [
    {
        "status": "applied",
        "summary": {
            "id": "d54ffb22",
            "paths": [
                "src/config.rs"
            ],
            "edits": [
                {
                    "kind": "replace",
                    "span": {
                        "unit": "rift://file/src/config.rs",
                        "range": {
                            "start": 162,
                            "end": 355
                        }
                    },
                    "text": "/// Loads the workspace configuration from `rift.toml`.\npub fn load_config(path: &Path) -> Result<Config, ConfigError> {\n    let text = std::fs::read_to_string(path)\n        .map_err(|error| ConfigError::read(path, error))?;\n    parse_config(&text)\n}"
                }
            ],
            "diagnostics": [
                {
                    "severity": "error",
                    "code": "rift.hook.failed",
                    "message": "hook format did not pass: exited 1; stderr (32 of 32 bytes): Diff in src/config.rs at line 12",
                    "related": [],
                    "tags": [],
                    "reliability": "reliable",
                    "continuation": "unknown",
                    "extensions": {}
                }
            ],
            "guarantees": [
                {
                    "kind": "behavior_checked",
                    "scope": {
                        "kind": "reach",
                        "reach": "project"
                    },
                    "hook": "tests",
                    "detail": "cargo test passes on the changed tree"
                }
            ]
        }
    },
    {
        "status": "refused",
        "reason": "unmet_precondition",
        "preconditions": [
            {
                "kind": "source_unchanged",
                "status": "failed",
                "addresses": [
                    {
                        "kind": "node",
                        "node": "rift://node/rust/src/config.rs@334-353#dd8aec0a"
                    }
                ],
                "paths": [
                    "src/config.rs"
                ],
                "expected": {
                    "kind": "boolean",
                    "value": true
                },
                "observed": {
                    "kind": "boolean",
                    "value": false
                }
            }
        ],
        "diagnostics": []
    }
]))]
pub enum ChangeResult {
    #[serde(rename = "applied")]
    /// The operation resolved to edits and Rift wrote them into the targeted tree.
    Applied {
        /// The applied change and its evidence.
        summary: ChangeSummary,
    },
    #[serde(rename = "refused")]
    /// Resolution produced no edits, so the targeted tree is untouched.
    Refused {
        /// The condition the caller can act on.
        reason: RefusalReason,
        /// Conditions checked before refusal, including at least one failed entry for
        /// `unmet_precondition`.
        #[schemars(length(max = 16))]
        preconditions: Vec<OperationPrecondition>,
        /// Evidence that explains the refusal.
        #[schemars(length(max = 256))]
        diagnostics: Vec<Diagnostic>,
    },
    /// A completed transform pipeline left the targeted tree unchanged.
    #[serde(rename = "unchanged")]
    Unchanged,
}

impl ChangeResult {
    /// A refusal carrying its checked conditions and no diagnostics.
    #[must_use]
    pub fn refused(reason: RefusalReason, preconditions: Vec<OperationPrecondition>) -> Self {
        Self::Refused {
            reason,
            preconditions,
            diagnostics: Vec::new(),
        }
    }
}

/// Longest inline body a `replace_symbol`, `insert_symbol`, or `replace_node` request
/// may carry, in UTF-8 bytes, and the longest a `file`-form body may resolve to. Equal
/// in value to the server's enforced rewrite-result bound, pinned by a conformance test
/// in `rift-server` - this crate cannot depend on the server crate to share the constant
/// directly.
pub const BODY_BYTES_MAX: usize = 1_048_576;

/// Where a write tool's body bytes come from: written inline in the request, or read
/// from a file. `patch`, and every write tool's `body`, accepts either form; the object
/// form is refused when it carries an unknown or additional field.
///
/// Both forms are bounded by the field that embeds this type; the schema states the
/// bound as the inline string's `maxLength`, and the server enforces the same bound
/// against the resolved content of either form.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum BodySource {
    /// The content itself.
    Inline(String),
    /// A file the server reads for the content.
    File {
        /// Absolute path to the file the server reads. Distinct from a destination
        /// path such as `insert_symbol`'s own `file`, which names where content
        /// lands, not where it comes from. The server reads this path itself, so a
        /// caller not co-located with the server cannot use this form.
        file: String,
    },
}

impl From<String> for BodySource {
    /// Wraps `value` as this source's inline content.
    fn from(value: String) -> Self {
        Self::Inline(value)
    }
}

impl From<&str> for BodySource {
    /// Wraps `value` as this source's inline content.
    fn from(value: &str) -> Self {
        Self::Inline(value.to_owned())
    }
}

/// Replaces one declaration addressed by symbol. The parser derives the span, so the
/// caller supplies no offsets. The whole declaration includes attached outer attributes
/// and doc comments.
///
/// `body` is spliced in verbatim at the declaration's own start byte: its first line
/// inherits the declaration's column, and every later line carries whatever indentation it
/// is written with.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.6"))]
#[schemars(extend("examples" = [
    {
        "symbol": "rift://symbol/rust/src/config.rs/load_config",
        "body": "/// Loads the workspace configuration from `rift.toml`.\npub fn load_config(path: &Path) -> Result<Config, ConfigError> {\n    let text = std::fs::read_to_string(path)\n        .map_err(|error| ConfigError::read(path, error))?;\n    parse_config(&text)\n}"
    }
]))]
#[schemars(transform = schema::declare_replace_symbol_body_length)]
pub struct ReplaceSymbolParams {
    /// The declaration identity returned by `get_symbol`.
    pub symbol: SymbolId,
    /// Which part of the declaration to replace. Omitted - the only form this release
    /// serves - replaces the whole declaration; a named region fails as
    /// `capability_unavailable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<RegionRole>,
    /// The replacement source, inline or read from a file.
    pub body: BodySource,
}

/// Inserts a new declaration beside an anchor symbol, or content at a file target.
/// The request carries exactly one of `anchor` or `file`; an anchored insertion lands
/// beside the whole declaration, attached outer attributes and doc comments included.
///
/// `body` inserted `before` its anchor is spliced in verbatim at the anchor's start byte, so
/// its first line inherits the anchor's column, and a blank line separates it from the
/// anchor that follows. The anchor's own leading indentation - spaces or tabs alone - is
/// copied back after that blank line, so the anchor keeps its column too; an anchor that
/// shares its line with other source keeps none. `body` inserted `after` its anchor, or at a
/// file target either side, always starts a fresh line at column zero, past the same
/// blank-line separator. The separator uses the source file's own line ending.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.6"))]
#[schemars(extend("examples" = [
    {
        "anchor": "rift://symbol/rust/src/config.rs/load_config",
        "position": "after",
        "body": "/// Renders the default configuration for a fresh workspace.\npub fn default_config() -> Config {\n    Config { root: std::path::PathBuf::from(\".\") }\n}",
        "create_missing": false
    }
]))]
#[schemars(transform = schema::insert_symbol_addresses_one_target)]
#[schemars(transform = schema::declare_insert_symbol_body_length)]
pub struct InsertSymbolParams {
    /// The existing declaration identity returned by `get_symbol`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<SymbolId>,
    /// The destination file the content lands in, created first when `create_missing`
    /// is set and it does not exist. Distinct from `body`'s own `file` form, which
    /// names a source the server reads the content from, not a destination.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<ProjectPath>,
    /// Which side of the anchor or file target receives the new content.
    pub position: InsertPosition,
    /// The new content: a declaration beside `anchor`, or a file target's whole body.
    /// Inline or read from a file.
    pub body: BodySource,
    /// Creates a missing `file` target instead of refusing. Invalid together with
    /// `anchor`.
    #[serde(default)]
    pub create_missing: bool,
}

/// Replaces one syntax node through a witnessed address from `nodes`. The server
/// recomputes the witness before writing and refuses when the bytes drifted.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.4"))]
#[schemars(extend("examples" = [
    {
        "node": "rift://node/rust/src/config.rs@334-353#4df4426e",
        "body": "parse_config(text.trim())"
    }
]))]
#[schemars(transform = schema::declare_replace_node_body_length)]
pub struct ReplaceNodeParams {
    /// The node identity returned by `nodes`, witness included.
    pub node: NodeId,
    /// Which named part of the node to replace. Omitted - the only form this release
    /// serves - replaces the node whole; a named region fails as
    /// `capability_unavailable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<RegionRole>,
    /// The replacement source, inline or read from a file.
    pub body: BodySource,
}

/// Inserts new content beside a syntax node addressed through a witnessed address from
/// `nodes`. The server recomputes the witness before writing and refuses when the bytes
/// drifted, the same check `replace_node` runs.
///
/// `body` lands verbatim at the node's own boundary, with no separator of its own: a node is
/// not a declaration, so `insert_symbol`'s blank-line spacing and column preservation do not
/// apply here, and the caller supplies whatever spacing the inserted bytes need.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.20"))]
#[schemars(extend("examples" = [
    {
        "anchor": "rift://node/rust/src/config.rs@334-353#4df4426e",
        "position": "after",
        "body": "\nparse_config(text.trim());"
    }
]))]
#[schemars(transform = schema::declare_insert_node_body_length)]
pub struct InsertNodeParams {
    /// The node identity returned by `nodes`, witness included.
    pub anchor: NodeId,
    /// Which side of the node receives the new content.
    pub position: InsertPosition,
    /// The new content, spliced in verbatim with no added separator. Inline or read from a
    /// file.
    pub body: BodySource,
}

/// Longest `new_name` a rename request may carry, in UTF-8 bytes.
pub const RENAME_NEW_NAME_BYTES_MAX: usize = 256;

/// Renames one declaration addressed by symbol through the configured language engine.
/// The engine proposes the edits; the server verifies each one against the tree and
/// writes them atomically, then reports surviving occurrences of the old name as
/// warnings. Refused as `unsupported` when no engine serves the declaration's language,
/// and `unmet_precondition` when the engine declines the rename or the source drifted.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.14"))]
#[schemars(extend("examples" = [
    {
        "symbol": "rift://symbol/rust/src/config.rs/load_config",
        "new_name": "read_config"
    }
]))]
pub struct RenameSymbolParams {
    /// The declaration identity returned by `get_symbol`.
    pub symbol: SymbolId,
    /// The declaration's new name. The engine judges identifier validity for its
    /// language; a name the engine refuses returns an `unmet_precondition` refusal
    /// carrying the engine's own words.
    #[schemars(length(min = 1, max = 256))]
    pub new_name: String,
}

impl RenameSymbolParams {
    /// Classifies `new_name` against the length its schema advertises. `schemars`
    /// constraints are declarative only - nothing enforces them at deserialization -
    /// so the server calls this before the name reaches an engine.
    #[must_use]
    pub fn new_name_violation(&self) -> Option<NewNameViolation> {
        match self.new_name.as_bytes() {
            [] => Some(NewNameViolation::Empty),
            bytes if bytes.len() > RENAME_NEW_NAME_BYTES_MAX => Some(NewNameViolation::TooLong),
            _ => None,
        }
    }
}

/// Reason a `new_name` spelling breaks the contract [`RenameSymbolParams`]'s schema
/// advertises.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NewNameViolation {
    /// The name is empty.
    Empty,
    /// The name is longer than [`RENAME_NEW_NAME_BYTES_MAX`] bytes.
    TooLong,
}

impl NewNameViolation {
    /// This violation's wire spelling, equal to its `Serialize` output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too_long",
        }
    }
}

/// Moves one visible file to a new project path. When the configured language engine
/// advertises will-rename requests for the file, the server asks it for reference
/// updates and applies them in the same atomic change; without an engine or the
/// capability the move still lands and the result carries a warning that references
/// were not updated.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.14"))]
#[schemars(extend("examples" = [
    {
        "from": "src/config.rs",
        "to": "src/settings.rs"
    }
]))]
pub struct MoveFileParams {
    /// The file to move, as a project-relative path.
    pub from: ProjectPath,
    /// The destination path. It must not exist yet; missing parent directories are
    /// created when the move lands.
    pub to: ProjectPath,
}

/// Removes one declaration addressed by symbol, checked against the configured language
/// engine's references first. The removed span reaches back over the declaration's attached
/// outer attributes and doc comments and forward over the separator that followed it, so no
/// blank-line run stands where the declaration stood.
///
/// With an engine advertising `textDocument/references`, a standing reference refuses
/// `unmet_precondition` naming `no_references` and the reference paths, unless `force`
/// applies the removal anyway and carries them as a warning instead. Without such an engine,
/// the removal applies and carries a warning naming why it was not checked.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.19"))]
#[schemars(extend("examples" = [
    {
        "symbol": "rift://symbol/rust/src/config.rs/default_config",
        "force": false
    }
]))]
pub struct RemoveSymbolParams {
    /// The declaration identity returned by `get_symbol`.
    pub symbol: SymbolId,
    /// Applies the removal even when references stand, carrying them as a warning instead
    /// of refusing.
    #[serde(default)]
    pub force: bool,
}

/// Removes one syntax node through a witnessed address from `nodes`, checked against the
/// configured language engine's references first when the node names a declaration. The
/// server recomputes the witness before writing and refuses when the bytes drifted.
///
/// The removed span reaches forward over the separator that followed the node, so no
/// blank-line run stands where it stood. A node naming no declaration is not checked: the
/// removal applies and carries a warning saying so. A node that does name one follows
/// `remove_symbol`'s reference check and its `force` override.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.19"))]
#[schemars(extend("examples" = [
    {
        "node": "rift://node/rust/src/config.rs@334-353#4df4426e",
        "force": false
    }
]))]
pub struct RemoveNodeParams {
    /// The node identity returned by `nodes`, witness included.
    pub node: NodeId,
    /// Applies the removal even when references stand, carrying them as a warning instead
    /// of refusing.
    #[serde(default)]
    pub force: bool,
}

/// Longest inline `patch` a request may carry, in UTF-8 bytes, and the longest a
/// `file`-form patch may resolve to.
pub const PATCH_BYTES_MAX: usize = 4_194_304;

/// Applies unified-diff hunks to workspace files atomically.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[schemars(extend("rift:since" = "v0.0.6"))]
#[schemars(extend("examples" = [
    {
        "patch": "--- a/src/config.rs\n+++ b/src/config.rs\n@@ -11,4 +11,4 @@\n pub fn load_config(path: &Path) -> Result<Config, ConfigError> {\n     let text = std::fs::read_to_string(path)?;\n-    parse_config(&text)\n+    parse_config(text.trim())\n }\n"
    }
]))]
#[schemars(transform = schema::declare_patch_body_length)]
pub struct PatchParams {
    /// A unified diff, inline or read from a file. The addressed file is any workspace
    /// file the `[source]` policy makes visible, whether or not a syntax provider
    /// parses it. Hunk context guards the change: a header's line numbers are hints
    /// and its line counts are read from the hunk's own body, as with `git apply`.
    /// `/dev/null` headers create or delete files.
    pub patch: BodySource,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::{Digest, FileId, TextRange};
    use schemars::schema_for;
    use serde_json::json;

    /// `ChangeId` and `Digest` share one eight-character wire form; this pins the two
    /// generated patterns to each other.
    #[test]
    fn change_id_schema_pattern_equals_the_digest_pattern() {
        let change_id = serde_json::to_value(schema_for!(ChangeId)).expect("change id schema");
        let digest = serde_json::to_value(schema_for!(Digest)).expect("digest schema");
        assert_eq!(change_id["pattern"], digest["pattern"]);
        assert_eq!(change_id["pattern"], json!(r"^[0-9a-f]{8}$"));
    }

    #[test]
    fn test_change_summary_schema_requires_paths_and_edits() {
        let schema = serde_json::to_value(schema_for!(ChangeSummary)).expect("change schema");
        assert_eq!(schema["properties"]["paths"]["minItems"], json!(1));
        assert_eq!(schema["properties"]["edits"]["minItems"], json!(1));
    }

    #[test]
    fn change_result_applied_round_trips_through_json() {
        let result = ChangeResult::Applied {
            summary: ChangeSummary {
                id: ChangeId("0123abcd".to_owned()),
                paths: vec![ProjectPath("src/lib.rs".to_owned())],
                edits: vec![Edit::Replace {
                    span: SourceSpan {
                        unit: FileId("rift://file/src%2Flib.rs".to_owned()),
                        range: TextRange { start: 0, end: 4 },
                    },
                    text: "fn f() {}".to_owned(),
                }],
                diagnostics: Vec::new(),
                guarantees: vec![GuaranteeEvidence {
                    kind: GuaranteeKind::BehaviorChecked,
                    scope: CoverageScope::Reach {
                        reach: crate::read::CoverageReach::Project,
                    },
                    hook: "tests".to_owned(),
                    detail: "cargo test passes on the changed tree".to_owned(),
                }],
            },
        };
        let value = serde_json::to_value(&result).expect("serialize");
        let round_tripped: ChangeResult = serde_json::from_value(value).expect("deserialize");
        assert_eq!(round_tripped, result);
    }

    #[test]
    fn change_result_refused_round_trips_through_json() {
        let result = ChangeResult::Refused {
            reason: RefusalReason::UnmetPrecondition,
            preconditions: vec![OperationPrecondition {
                kind: OperationPreconditionKind::SourceUnchanged,
                status: OperationPreconditionStatus::Failed,
                addresses: vec![PreconditionAddress::Symbol {
                    symbol: SymbolId("rift://symbol/rust/foo".to_owned()),
                }],
                paths: vec![ProjectPath("src/lib.rs".to_owned())],
                expected: PreconditionValue::Boolean { value: true },
                observed: PreconditionValue::Boolean { value: false },
            }],
            diagnostics: Vec::new(),
        };
        let value = serde_json::to_value(&result).expect("serialize");
        let round_tripped: ChangeResult = serde_json::from_value(value).expect("deserialize");
        assert_eq!(round_tripped, result);
    }

    #[test]
    fn edit_kind_serializes_to_exact_wire_string() {
        let edit = Edit::Replace {
            span: SourceSpan {
                unit: FileId("rift://file/src%2Flib.rs".to_owned()),
                range: TextRange { start: 0, end: 0 },
            },
            text: String::new(),
        };
        let value = serde_json::to_value(&edit).expect("serialize");
        assert_eq!(value["kind"], json!("replace"));
    }

    #[test]
    fn replace_symbol_params_without_region_deserializes_to_none() {
        let params: ReplaceSymbolParams = serde_json::from_value(json!({
            "symbol": "rift://symbol/rust/foo",
            "body": "fn foo() {}"
        }))
        .expect("deserialize");
        assert_eq!(params.region, None);
    }

    #[test]
    fn replace_symbol_params_rejects_projection_field() {
        let result = serde_json::from_value::<ReplaceSymbolParams>(json!({
            "symbol": "rift://symbol/rust/foo",
            "body": "fn foo() {}",
            "projection": "rift://projection/my-feature-one"
        }));
        assert!(result.is_err());
    }

    /// `body` accepts an inline string, decoding to [`BodySource::Inline`].
    #[test]
    fn replace_symbol_params_body_accepts_the_inline_form() {
        let params: ReplaceSymbolParams = serde_json::from_value(json!({
            "symbol": "rift://symbol/rust/foo",
            "body": "fn foo() {}"
        }))
        .expect("inline body deserializes");
        assert_eq!(params.body, BodySource::Inline("fn foo() {}".to_owned()));
    }

    /// `body` accepts an object naming a `file`, decoding to [`BodySource::File`].
    #[test]
    fn replace_symbol_params_body_accepts_the_file_form() {
        let params: ReplaceSymbolParams = serde_json::from_value(json!({
            "symbol": "rift://symbol/rust/foo",
            "body": {"file": "/tmp/change.diff"}
        }))
        .expect("file body deserializes");
        assert_eq!(
            params.body,
            BodySource::File {
                file: "/tmp/change.diff".to_owned()
            }
        );
    }

    /// A `body` object carrying an unknown field alongside `file` matches neither
    /// `BodySource` variant, so the request refuses before disk or engine is touched.
    #[test]
    fn body_source_file_form_rejects_an_unknown_field() {
        let result = serde_json::from_value::<ReplaceSymbolParams>(json!({
            "symbol": "rift://symbol/rust/foo",
            "body": {"file": "/tmp/change.diff", "extra": 1}
        }));
        assert!(result.is_err());
    }

    /// `InsertNodeParams` decodes `anchor`, `position`, and `body`, and rejects a request
    /// carrying an unknown field alongside them.
    #[test]
    fn insert_node_params_decodes_required_fields_and_rejects_unknown_fields() {
        let params: InsertNodeParams = serde_json::from_value(json!({
            "anchor": "rift://node/rust/foo.rs@0-1#00000000",
            "position": "after",
            "body": "fn b() {}"
        }))
        .expect("insert_node params deserialize");
        assert_eq!(params.position, InsertPosition::After);
        assert_eq!(params.body, BodySource::Inline("fn b() {}".to_owned()));

        let result = serde_json::from_value::<InsertNodeParams>(json!({
            "anchor": "rift://node/rust/foo.rs@0-1#00000000",
            "position": "after",
            "body": "fn b() {}",
            "extra": 1
        }));
        assert!(result.is_err());
    }

    /// A `body` object naming no `file` at all matches neither variant either.
    #[test]
    fn body_source_rejects_an_empty_object() {
        let result = serde_json::from_value::<ReplaceSymbolParams>(json!({
            "symbol": "rift://symbol/rust/foo",
            "body": {}
        }));
        assert!(result.is_err());
    }

    /// The schema's `length` literals restate [`RENAME_NEW_NAME_BYTES_MAX`], because
    /// attribute arguments take only literals; this pins the two to each other.
    #[test]
    fn rename_new_name_schema_length_pins_the_enforced_bound() {
        let schema =
            serde_json::to_value(schema_for!(RenameSymbolParams)).expect("rename params schema");
        let new_name = &schema["properties"]["new_name"];
        assert_eq!(new_name["minLength"], json!(1));
        assert_eq!(
            new_name["maxLength"],
            json!(RENAME_NEW_NAME_BYTES_MAX),
            "the advertised length must equal the enforced constant"
        );
    }

    #[test]
    fn rename_new_name_violations_classify_empty_and_oversized() {
        let params = |new_name: String| RenameSymbolParams {
            symbol: SymbolId("rift://symbol/rust/lib.rs/beacon".to_owned()),
            new_name,
        };
        assert_eq!(
            params(String::new()).new_name_violation(),
            Some(NewNameViolation::Empty)
        );
        assert_eq!(NewNameViolation::Empty.as_str(), "empty");
        let oversized = params("n".repeat(RENAME_NEW_NAME_BYTES_MAX + 1));
        assert_eq!(
            oversized.new_name_violation(),
            Some(NewNameViolation::TooLong)
        );
        assert_eq!(NewNameViolation::TooLong.as_str(), "too_long");
        let at_bound = params("n".repeat(RENAME_NEW_NAME_BYTES_MAX));
        assert_eq!(at_bound.new_name_violation(), None);
        assert_eq!(params("renamed".to_owned()).new_name_violation(), None);
    }

    /// The wire spelling and `as_str` come from one serde declaration; this
    /// pins the two together for every variant.
    #[test]
    fn new_name_violation_spellings_match_their_serialization() {
        for violation in [NewNameViolation::Empty, NewNameViolation::TooLong] {
            let serialized = serde_json::to_value(violation).expect("violations serialize");
            assert_eq!(serialized, json!(violation.as_str()));
        }
    }
}
