//! Wire models for provider, hook, and apply findings (`Diagnostic` and its context).
//! Extracted from [`crate::read`] so that module stays below its size bound; every type here is
//! re-exported from `read` so existing `rift_protocol::read::Diagnostic`-style paths keep
//! resolving.

use crate::read::{Extensions, Language, Severity, SourceExcerpt, SourceSpan};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One finding a provider produced from source. Its code and message retain the provider's
/// vocabulary.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    /// How much it matters.
    pub severity: Severity,
    /// The provider's own identifier for this finding - `TS2345`, `E0308`. Absent where
    /// the provider issues none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// What the provider said, in its own words.
    #[schemars(length(max = 4096))]
    pub message: String,
    /// Where it applies. Absent for a finding about the file as a whole, or about the
    /// build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Other places the provider pointed at while explaining this one.
    pub related: Vec<DiagnosticRelated>,
    /// Presentation tags for the finding. A consumer can render them as strikethrough or
    /// grey text.
    pub tags: Vec<DiagnosticTag>,
    /// Whether the facts around this finding came off a clean parse.
    pub reliability: DiagnosticReliability,
    /// Whether the finding is an artefact of source that stops mid-way, which is the normal
    /// state of a file the caller is halfway through writing.
    pub continuation: DiagnosticContinuation,
    /// Diagnostic fields the model has no place for, namespaced by the provider that
    /// emitted them.
    pub extensions: Extensions,
    /// The language whose provider produced this. Absent for a finding Rift itself
    /// raised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
}

/// A code Rift stamps on a diagnostic it authors itself, as `Diagnostic.code`. Provider
/// diagnostics keep their tools' own codes; these name the findings the server raises about a
/// change it applied. The code strings are owned by serde, the same way error codes are.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub enum DiagnosticCode {
    /// A configured hook did not pass over an applied change.
    #[serde(rename = "rift.hook.failed")]
    HookFailed,
    /// Snapshot rebuild failed after an applied change; current-tree reads refuse until the
    /// server publishes a fresh snapshot.
    #[serde(rename = "rift.snapshot.stale")]
    SnapshotStale,
    /// A word-boundary occurrence of a renamed declaration's old name survives in the
    /// changed tree after the rename.
    #[serde(rename = "rift.rename.survivor")]
    RenameSurvivor,
}

impl DiagnosticCode {
    /// The stable code string, read back through serialization so it cannot drift from the
    /// serde spelling.
    #[must_use]
    pub fn code(self) -> String {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::String(code)) => code,
            other => unreachable!(
                "diagnostic codes are unit variants and must serialize to plain strings, \
                 got {other:?}"
            ),
        }
    }
}

/// One `Diagnostic` as an MCP answer carries it: the fact its emitter minted, plus what Rift
/// can add on top - where it lands in a line and column, and the source around it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticContext {
    /// Component that produced the diagnostic.
    pub source: DiagnosticContextSource,
    /// The finding itself, exactly as its emitter minted it.
    pub diagnostic: Diagnostic,
    /// One-based line the finding starts on. Absent where the diagnostic has no span - a
    /// whole-project error has nowhere to point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    /// One-based column within that line, counted in UTF-8 bytes. Absent for the same
    /// reason as `line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u64>,
    /// The source the finding points at. Absent where there is no span to copy from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<SourceExcerpt>,
}

/// Component that produced the diagnostic. Rift sets this after collecting provider, hook,
/// or apply output.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticContextSource {
    /// A language provider emitted the finding.
    Provider,
    /// A configured hook emitted the finding.
    Hook,
    /// Applying a change request emitted the finding.
    Apply,
}

/// Whether the finding is an artefact of source that stops mid-way, which is the normal
/// state of a file the caller is halfway through writing.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticContinuation {
    /// The producer recovered and later facts remain reliable.
    Repairable,
    /// The producer could not recover, so later facts are suspect.
    Unrepairable,
    /// The producer does not say whether it recovered.
    Unknown,
}

/// A second place the provider points at - the earlier declaration a redefinition conflicts
/// with, the bound that failed. It carries a message and a location, and never a severity of
/// its own, because it is part of one finding.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticRelated {
    /// What to notice there - "first defined here", "required by this bound".
    #[schemars(length(max = 4096))]
    pub message: String,
    /// Where to look.
    pub span: SourceSpan,
}

/// Whether the facts around this finding came off a clean parse.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticReliability {
    /// The surrounding facts came off a clean parse.
    Reliable,
    /// The parser recovered nearby, so surrounding facts may be off.
    Recovered,
}

/// Presentation tags for the finding. A consumer can render them as strikethrough or grey
/// text.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticTag {
    /// The code the finding points at is deprecated.
    Deprecated,
    /// The code the finding points at is unused or has no effect.
    Unnecessary,
}

#[cfg(test)]
mod tests {
    use super::DiagnosticCode;

    #[test]
    fn test_diagnostic_codes_serialize_to_their_documented_spellings() {
        assert_eq!(DiagnosticCode::HookFailed.code(), "rift.hook.failed");
        assert_eq!(DiagnosticCode::SnapshotStale.code(), "rift.snapshot.stale");
        assert_eq!(
            DiagnosticCode::RenameSurvivor.code(),
            "rift.rename.survivor"
        );
    }
}
