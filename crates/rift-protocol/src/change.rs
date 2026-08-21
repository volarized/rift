//! Wire models for the Rift MCP change tools.
//!
//! Every type here is a wire contract: serde attributes define exactly what
//! the server accepts and returns, and the MCP server derives its advertised
//! request and response schemas from these definitions.

use crate::configuration::GuaranteeKind;
use crate::read::{
    CoverageScope, Diagnostic, NodeId, ProjectPath, RegionRole, SourceSpan, SymbolId,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Identity of one applied change, minted when the change lands.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
#[schemars(transparent)]
pub struct ChangeId(
    #[schemars(length(min = 30, max = 30))]
    #[schemars(regex(pattern = r"^chg_[a-z2-7]{26}$"))]
    pub String,
);

/// Which side of the anchor receives the new declaration.
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
    /// The address resolves to several targets. Narrow it and ask again.
    #[serde(rename = "ambiguous_target")]
    AmbiguousTarget,
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
    #[schemars(length(max = 256))]
    pub paths: Vec<ProjectPath>,
    /// Concrete edits in canonical file-and-range order.
    #[schemars(length(max = 256))]
    pub edits: Vec<Edit>,
    /// Resolution findings in source order, then one finding per hook that
    /// did not pass.
    #[schemars(length(max = 256))]
    pub diagnostics: Vec<Diagnostic>,
    /// Properties the workspace's passing hooks established about this
    /// change, in hook list order.
    #[schemars(length(max = 512))]
    pub guarantees: Vec<GuaranteeEvidence>,
}

/// An applied change or semantic refusal. For every refusal the targeted tree is
/// unchanged.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", deny_unknown_fields)]
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

/// Replaces one declaration addressed by symbol. The parser derives the span, so the
/// caller supplies no offsets.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceSymbolParams {
    /// The declaration to replace.
    pub symbol: SymbolId,
    /// Which part of the declaration to replace. Null — the only value this release
    /// serves — replaces the whole declaration; a named region fails as
    /// `capability_unavailable`.
    #[serde(default)]
    pub region: Option<RegionRole>,
    /// The replacement source.
    #[schemars(length(max = 1_048_576))]
    pub body: String,
}

/// Inserts a new declaration beside an existing one, addressed by its anchor symbol.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InsertSymbolParams {
    /// The existing declaration the new one lands beside.
    pub anchor: SymbolId,
    /// Which side of the anchor receives the new declaration.
    pub position: InsertPosition,
    /// The new declaration's source.
    #[schemars(length(max = 1_048_576))]
    pub body: String,
}

/// Replaces one syntax node through a witnessed address from `nodes`. The server
/// recomputes the witness before writing and refuses when the bytes drifted.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceNodeParams {
    /// The node to replace, witness included.
    pub node: NodeId,
    /// Which named part of the node to replace. Null — the only value this release
    /// serves — replaces the node whole; a named region fails as
    /// `capability_unavailable`.
    #[serde(default)]
    pub region: Option<RegionRole>,
    /// The replacement source.
    #[schemars(length(max = 1_048_576))]
    pub body: String,
}

/// Applies unified-diff hunks to workspace files atomically.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PatchParams {
    /// A unified diff. Hunk context guards the change; header line numbers are hints,
    /// as with `git apply`. `/dev/null` headers create or delete files.
    #[schemars(length(min = 1, max = 4_194_304))]
    pub patch: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::{FileId, TextRange};
    use serde_json::json;

    #[test]
    fn change_result_applied_round_trips_through_json() {
        let result = ChangeResult::Applied {
            summary: ChangeSummary {
                id: ChangeId("chg_bbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned()),
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
            "projection": null
        }));
        assert!(result.is_err());
    }
}
