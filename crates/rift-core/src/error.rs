//! Rift's error registry: the full set of errors that can happen across the
//! MCP and CLI surfaces.
//!
//! The wire piece is [`rift_protocol::error::ErrorCode`]; this module adds
//! the CLI-only piece, the canonical explanation and action for every
//! identity, and the generic [`Error`] carrier every crate renders failures
//! through. Code strings are owned by serde on the code enums, so the
//! registry and the wire cannot drift.

use std::fmt;

pub use rift_protocol::error::{ErrorCode, RetryDirective};
use serde::Serialize;
use strum::VariantArray;

/// CLI-only failure identities. They never cross the MCP wire: the update
/// and artifact commands raise them directly to an operator.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, VariantArray)]
#[serde(rename_all = "snake_case")]
pub enum CliCode {
    /// Installed rift binary cannot be inspected.
    UpdateBinaryInvalid,
    /// Published release does not match the expected form.
    UpdateReleaseInvalid,
    /// Release could not be downloaded.
    UpdateDownloadFailed,
    /// Update staging directory could not be prepared.
    UpdateStagingFailed,
    /// Downloaded release does not match its published checksum.
    UpdateChecksumMismatch,
    /// Downloaded release archive is not valid.
    UpdateArchiveInvalid,
    /// New rift binary could not be installed.
    UpdatePublishFailed,
    /// Previous rift binary could not be restored.
    UpdateRollbackFailed,
    /// Generated artifact no longer matches its source.
    ArtifactStale,
}

/// Stable machine-readable error identity: a wire code or a CLI-only code.
///
/// Every operating failure Rift raises to a user or agent resolves to one of
/// these. The wire surface serializes the [`ErrorCode`] directly; a CLI-only
/// identity never leaves the process over MCP.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ErrorName {
    /// An identity the MCP error surface serializes as `ErrorData.code`.
    Wire(ErrorCode),
    /// An identity only the CLI renders.
    Cli(CliCode),
}

impl ErrorName {
    /// Every registered identity, wire codes first, in declaration order.
    pub fn all() -> impl Iterator<Item = Self> {
        let wire = ErrorCode::VARIANTS.iter().copied().map(Self::Wire);
        let cli = CliCode::VARIANTS.iter().copied().map(Self::Cli);
        wire.chain(cli)
    }

    /// The stable `snake_case` code string, serialized by the code enums
    /// themselves.
    #[must_use]
    pub fn code(self) -> String {
        let value = match self {
            Self::Wire(code) => serde_json::to_value(code),
            Self::Cli(code) => serde_json::to_value(code),
        };
        match value {
            Ok(serde_json::Value::String(code)) => code,
            other => unreachable!(
                "error identities are unit variants and must serialize to \
                 plain strings, got {other:?}"
            ),
        }
    }

    /// Canonical metadata for this identity: explanation, default retry
    /// guidance, and the suggested action.
    #[must_use]
    pub const fn descriptor(self) -> ErrorDescriptor {
        let (explanation, retry, action) = match self {
            Self::Wire(code) => wire_guidance(code),
            Self::Cli(code) => cli_guidance(code),
        };
        ErrorDescriptor {
            name: self,
            explanation,
            retry,
            action,
        }
    }
}

/// The registry table for wire identities.
const fn wire_guidance(code: ErrorCode) -> (&'static str, RetryDirective, &'static str) {
    match code {
        ErrorCode::InvalidRequest => (
            "the request does not match the documented form",
            RetryDirective::Never,
            "correct the reported field and resend the request",
        ),
        ErrorCode::PermissionDenied => (
            "the request addresses state outside what the workspace allows",
            RetryDirective::Never,
            "address paths inside the workspace root, without symlink components",
        ),
        ErrorCode::ResourceNotFound => (
            "the requested resource does not exist in the current snapshot",
            RetryDirective::OperatorAction,
            "search or list first, then retry with an identity that answer returned",
        ),
        ErrorCode::ContentUnavailable => (
            "the addressed content exists but its bytes cannot be served",
            RetryDirective::Never,
            "request the declaration without its body, or read a source-backed unit",
        ),
        ErrorCode::CursorInvalid => (
            "the cursor does not continue the request it was sent with",
            RetryDirective::Never,
            "page with the cursor exactly as returned, on the request that minted it",
        ),
        ErrorCode::CursorExpired => (
            "the cursor's captured results are no longer retained",
            RetryDirective::Never,
            "restart the request from its first page",
        ),
        ErrorCode::Cancelled => (
            "the request was cancelled before it completed",
            RetryDirective::SameRequest,
            "resend the request if the result is still needed",
        ),
        ErrorCode::LimitExceeded => (
            "the request exceeded a declared resource limit",
            RetryDirective::Never,
            "resize the request below the named limit, or raise that limit in the workspace configuration",
        ),
        ErrorCode::StorageFailure => (
            "workspace state could not be read or written",
            RetryDirective::SameRequest,
            "check filesystem permissions and free space, then retry",
        ),
        ErrorCode::InternalError => (
            "the server failed in a way it did not classify",
            RetryDirective::SameRequest,
            "retry once, and report the full message if the failure repeats",
        ),
        ErrorCode::UnsupportedPath => (
            "the path cannot be addressed by this workspace",
            RetryDirective::Never,
            "use a workspace-relative path with `/` separators and no `.` or `..` components",
        ),
        ErrorCode::TemporarilyUnavailable => (
            "the server cannot serve this request yet",
            RetryDirective::SameRequest,
            "resend the same request after a short delay",
        ),
        ErrorCode::ConfigurationInvalid => (
            "the workspace configuration failed validation",
            RetryDirective::OperatorAction,
            "correct the reported configuration field, then retry",
        ),
        ErrorCode::CapabilityUnavailable => (
            "no configured provider serves this request",
            RetryDirective::OperatorAction,
            "adjust the request to a served capability, or configure a provider that serves it",
        ),
    }
}

/// The registry table for CLI-only identities.
const fn cli_guidance(code: CliCode) -> (&'static str, RetryDirective, &'static str) {
    match code {
        CliCode::UpdateBinaryInvalid => (
            "the installed rift binary cannot be inspected",
            RetryDirective::OperatorAction,
            "reinstall rift from an official release",
        ),
        CliCode::UpdateReleaseInvalid => (
            "the published release does not match the expected form",
            RetryDirective::SameRequest,
            "retry `rift update`, and report the release if the failure repeats",
        ),
        CliCode::UpdateDownloadFailed => (
            "the release could not be downloaded",
            RetryDirective::SameRequest,
            "check network connectivity, then retry `rift update`",
        ),
        CliCode::UpdateStagingFailed => (
            "the update staging directory could not be prepared",
            RetryDirective::OperatorAction,
            "ensure the temporary directory is writable and has free space, then retry `rift update`",
        ),
        CliCode::UpdateChecksumMismatch => (
            "the downloaded release does not match its published checksum",
            RetryDirective::SameRequest,
            "retry `rift update`, and report the release if the mismatch repeats",
        ),
        CliCode::UpdateArchiveInvalid => (
            "the downloaded release archive is not valid",
            RetryDirective::SameRequest,
            "retry `rift update`, and report the release if the failure repeats",
        ),
        CliCode::UpdatePublishFailed => (
            "the new rift binary could not be installed",
            RetryDirective::OperatorAction,
            "ensure the install directory is writable, then retry `rift update`",
        ),
        CliCode::UpdateRollbackFailed => (
            "the previous rift binary could not be restored",
            RetryDirective::OperatorAction,
            "reinstall rift from an official release",
        ),
        CliCode::ArtifactStale => (
            "a generated artifact no longer matches its source",
            RetryDirective::OperatorAction,
            "regenerate the artifact with the printed command",
        ),
    }
}

/// Canonical user-facing error metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorDescriptor {
    name: ErrorName,
    explanation: &'static str,
    retry: RetryDirective,
    action: &'static str,
}

impl ErrorDescriptor {
    /// Returns stable symbolic identity.
    #[must_use]
    pub const fn name(self) -> ErrorName {
        self.name
    }

    /// Returns the stable code string for this identity.
    #[must_use]
    pub fn code(self) -> String {
        self.name.code()
    }

    /// Returns canonical explanation.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        self.explanation
    }

    /// Returns default retry guidance.
    #[must_use]
    pub const fn retry(self) -> RetryDirective {
        self.retry
    }

    /// Returns canonical suggested action.
    #[must_use]
    pub const fn action(self) -> &'static str {
        self.action
    }
}

/// Typed detail attached to one operating failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorContext {
    key: &'static str,
    value: String,
}

impl ErrorContext {
    /// Constructs context with stable key and display value.
    #[must_use]
    pub fn new(key: &'static str, value: impl Into<String>) -> Self {
        Self {
            key,
            value: value.into(),
        }
    }

    /// Returns stable context key.
    #[must_use]
    pub const fn key(&self) -> &'static str {
        self.key
    }

    /// Returns context value.
    #[must_use]
    pub fn value(&self) -> &str {
        self.value.as_str()
    }
}

/// Prepares a detailed failure message from the error and its context.
///
/// The message always outlines what failed, which values caused the failure,
/// and the recommended next step, in one form every surface prints:
/// `explanation: key value, key value; action`. This render is the
/// human-readable form; the same information travels as JSON in `ErrorData`.
#[must_use]
pub fn render_failure(descriptor: ErrorDescriptor, context: &[ErrorContext]) -> String {
    let evidence = context
        .iter()
        .map(|entry| format!("{} {}", entry.key(), entry.value()))
        .collect::<Vec<_>>()
        .join(", ");
    if evidence.is_empty() {
        format!("{}; {}", descriptor.explanation(), descriptor.action())
    } else {
        format!(
            "{}: {}; {}",
            descriptor.explanation(),
            evidence,
            descriptor.action()
        )
    }
}

/// A failure kind that resolves to one registry identity and carries its own
/// typed evidence.
///
/// Domain crates implement this on their kind enums — including violation
/// enums, which are just fault kinds — and expose [`Error`] over them, so
/// classification, rendering, and source exposure follow one rule everywhere.
pub trait Fault: fmt::Debug {
    /// The registry identity this kind classifies as.
    fn name(&self) -> ErrorName;

    /// Typed evidence this kind carries, outermost first.
    fn context(&self) -> Vec<ErrorContext> {
        Vec::new()
    }

    /// The underlying failure this kind wraps, where one exists.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

/// The serde name of a fault kind's variant: a unit variant serializes to a
/// string, a payload variant to a single-key map. Fault kinds derive
/// `Serialize` with `rename_all = "snake_case"` and use this as their label,
/// so a kind's label and its wire spelling cannot drift. A value serde does
/// not name falls back to its `Debug` form.
#[must_use]
pub fn fault_label<K: Serialize + fmt::Debug>(kind: &K) -> String {
    match serde_json::to_value(kind) {
        Ok(serde_json::Value::String(label)) => label,
        Ok(serde_json::Value::Object(map)) => match map.into_iter().next() {
            Some((label, _)) => label,
            None => format!("{kind:?}"),
        },
        _ => format!("{kind:?}"),
    }
}

/// An operating failure: a fault kind plus call-site evidence.
///
/// Every crate's public error is this type over its own [`Fault`] kind, so
/// context threading, rendering, and `source` exist once. Display prints the
/// registry rule for the kind's identity. Equality follows the fault kind:
/// it is derived, so it exists exactly where the kind supports it.
#[derive(Debug, PartialEq, Eq)]
pub struct Error<K: Fault> {
    kind: K,
    context: Vec<ErrorContext>,
}

impl<K: Fault> Error<K> {
    /// Constructs the failure for one fault kind.
    #[must_use]
    pub fn new(kind: K) -> Self {
        Self {
            kind,
            context: Vec::new(),
        }
    }

    /// Attaches call-site evidence, rendered after the kind's own.
    #[must_use]
    pub fn with_context(mut self, context: ErrorContext) -> Self {
        self.context.push(context);
        self
    }

    /// Returns the fault kind.
    #[must_use]
    pub fn fault(&self) -> &K {
        &self.kind
    }

    /// Returns the registry identity.
    #[must_use]
    pub fn name(&self) -> ErrorName {
        self.kind.name()
    }

    /// Returns canonical registry metadata.
    #[must_use]
    pub fn descriptor(&self) -> ErrorDescriptor {
        self.name().descriptor()
    }

    /// Returns the kind's evidence followed by call-site evidence.
    #[must_use]
    pub fn context(&self) -> Vec<ErrorContext> {
        let mut context = self.kind.context();
        context.extend(self.context.iter().cloned());
        context
    }
}

impl<K: Fault> From<K> for Error<K> {
    fn from(kind: K) -> Self {
        Self::new(kind)
    }
}

impl<K: Fault> fmt::Display for Error<K> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&render_failure(self.descriptor(), &self.context()))
    }
}

impl<K: Fault> std::error::Error for Error<K> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.kind.source()
    }
}

impl Fault for ErrorName {
    fn name(&self) -> ErrorName {
        *self
    }
}

/// A failure with no domain kind beyond its registry identity.
pub type RiftError = Error<ErrorName>;

#[cfg(test)]
mod tests {
    use super::{
        CliCode, Error, ErrorCode, ErrorContext, ErrorName, Fault, RiftError, fault_label,
        render_failure,
    };
    use serde::Serialize;
    use std::collections::HashSet;
    use strum::VariantArray;

    #[test]
    fn registry_codes_are_unique_snake_case_strings() {
        let mut codes = HashSet::new();
        let mut count = 0_usize;
        for name in ErrorName::all() {
            let code = name.code();
            assert!(
                codes.insert(code.clone()),
                "registry code must be unique: {code} appears twice"
            );
            assert!(
                code.chars()
                    .all(|character| character.is_ascii_lowercase() || character == '_'),
                "registry code must be snake_case: {code}"
            );
            count += 1;
        }
        assert_eq!(
            count,
            ErrorCode::VARIANTS.len() + CliCode::VARIANTS.len(),
            "the registry iterates every wire and CLI identity exactly once"
        );
    }

    #[test]
    fn code_strings_come_from_serde() {
        assert_eq!(
            ErrorName::Wire(ErrorCode::LimitExceeded).code(),
            "limit_exceeded"
        );
        assert_eq!(
            ErrorName::Cli(CliCode::ArtifactStale).code(),
            "artifact_stale"
        );
    }

    #[test]
    fn every_descriptor_explains_and_directs() {
        for name in ErrorName::all() {
            let descriptor = name.descriptor();
            assert!(
                !descriptor.explanation().is_empty() && !descriptor.action().is_empty(),
                "descriptor for {name:?} must carry an explanation and an action"
            );
            assert!(
                descriptor
                    .explanation()
                    .chars()
                    .next()
                    .is_some_and(char::is_lowercase)
                    && !descriptor.explanation().ends_with('.'),
                "explanation for {name:?} must be lowercase without trailing punctuation"
            );
        }
    }

    #[test]
    fn rendered_failure_names_values_and_action() {
        let error = RiftError::new(ErrorName::Wire(ErrorCode::LimitExceeded))
            .with_context(ErrorContext::new("limit", "search.results_max 100"))
            .with_context(ErrorContext::new("required", "250"));
        assert_eq!(
            error.to_string(),
            "the request exceeded a declared resource limit: \
             limit search.results_max 100, required 250; \
             resize the request below the named limit, or raise that limit \
             in the workspace configuration"
        );
    }

    #[test]
    fn rendered_failure_without_context_keeps_explanation_and_action() {
        let rendered = render_failure(ErrorName::Wire(ErrorCode::CursorExpired).descriptor(), &[]);
        assert_eq!(
            rendered,
            "the cursor's captured results are no longer retained; \
             restart the request from its first page"
        );
    }

    #[derive(Debug, Serialize)]
    #[serde(rename_all = "snake_case")]
    enum ProbeFault {
        MissingTarget { path: String },
        Unreadable,
    }

    impl Fault for ProbeFault {
        fn name(&self) -> ErrorName {
            ErrorName::Wire(ErrorCode::ResourceNotFound)
        }

        fn context(&self) -> Vec<ErrorContext> {
            let mut context = Vec::new();
            context.push(ErrorContext::new("fault", fault_label(self)));
            if let Self::MissingTarget { path } = self {
                context.push(ErrorContext::new("path", path.clone()));
            }
            context
        }
    }

    #[test]
    fn generic_error_merges_kind_and_call_site_context_in_order() {
        let error = Error::new(ProbeFault::MissingTarget {
            path: "src/lib.rs".to_owned(),
        })
        .with_context(ErrorContext::new("operation", "replace_symbol"));
        let context = error.context();
        let keys: Vec<&str> = context.iter().map(ErrorContext::key).collect();
        assert_eq!(keys, ["fault", "path", "operation"]);
        assert_eq!(
            error.to_string(),
            "the requested resource does not exist in the current snapshot: \
             fault missing_target, path src/lib.rs, operation replace_symbol; \
             search or list first, then retry with an identity that answer returned"
        );
    }

    #[test]
    fn fault_labels_come_from_serde_for_unit_and_payload_variants() {
        assert_eq!(fault_label(&ProbeFault::Unreadable), "unreadable");
        assert_eq!(
            fault_label(&ProbeFault::MissingTarget {
                path: String::new()
            }),
            "missing_target"
        );
    }

    #[derive(Debug, Serialize)]
    struct Hollow {}

    #[test]
    fn fault_labels_fall_back_to_debug_for_unnamed_values() {
        assert_eq!(fault_label(&7_u8), "7");
        assert_eq!(fault_label(&Hollow {}), "Hollow");
    }

    #[derive(Debug)]
    struct SourcedFault(std::io::Error);

    impl Fault for SourcedFault {
        fn name(&self) -> ErrorName {
            ErrorName::Wire(ErrorCode::StorageFailure)
        }

        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn generic_error_exposes_the_kind_source() {
        let error = Error::from(SourcedFault(std::io::Error::other("disk gone")));
        let source = std::error::Error::source(&error).expect("source must be exposed");
        assert_eq!(source.to_string(), "disk gone");
    }
}
