//! Wire projection of operating failures, hook findings, and stale-snapshot
//! findings served on tool results.

use std::fmt::Write as _;

use rift_core::{ErrorName, Fault};
use rift_protocol::configuration::CommandHook;
use rift_protocol::error as wire;
use rift_protocol::read::DiagnosticCode;
use rift_server::{HookRun, HookStatus, ReadError};
use rmcp::ErrorData;
use rmcp::model::ErrorCode;

/// JSON-RPC error code every Rift operating failure travels under: the
/// first code of the server-defined range (-32000 to -32099), which rmcp
/// exports no constant for — its constants name only MCP-defined codes. The
/// machine-readable classification is the [`wire::ErrorData`] in `data`.
pub(crate) const RIFT_ERROR_CODE: ErrorCode = ErrorCode(-32000);

/// Most `causes` entries one wire error carries, matching the advertised
/// schema bound.
pub(crate) const ERROR_CAUSES_MAX: usize = 8;

/// Bytes of each captured hook stream a failure finding quotes. The finding
/// also states the full sizes, so a truncated quote stays distinguishable
/// from a short log.
pub(crate) const HOOK_FINDING_STREAM_BYTES_MAX: usize = 1_024;

/// The finding an applied change carries for one hook that did not pass:
/// what ended the run, then each non-empty stream's size and bounded quote.
pub(crate) fn hook_failure_diagnostic(
    hook: &CommandHook,
    run: &HookRun,
) -> rift_protocol::read::Diagnostic {
    let account = match &run.status {
        HookStatus::Passed => unreachable!(
            "a passing hook contributes guarantees, not findings: hook={:?}",
            hook.id
        ),
        HookStatus::Failed => match run.exit_code {
            Some(code) => format!("exited {code}"),
            None => "exited nonzero".to_owned(),
        },
        HookStatus::TimedOut => format!("killed after {}ms", hook.timeout.milliseconds()),
        HookStatus::Error(message) => message.clone(),
    };
    let mut message = format!("hook {} did not pass: {account}", hook.id);
    for (stream_name, stream) in [("stdout", &run.stdout), ("stderr", &run.stderr)] {
        if stream.total_bytes == 0 {
            continue;
        }
        let quoted = bounded_prefix(&stream.text, HOOK_FINDING_STREAM_BYTES_MAX);
        let _ = write!(
            message,
            "; {stream_name} ({} of {} bytes): {quoted}",
            quoted.len(),
            stream.total_bytes,
        );
    }
    rift_protocol::read::Diagnostic {
        severity: rift_protocol::read::Severity::Error,
        code: Some(DiagnosticCode::HookFailed.code()),
        message,
        span: None,
        related: Vec::new(),
        tags: Vec::new(),
        reliability: rift_protocol::read::DiagnosticReliability::Reliable,
        continuation: rift_protocol::read::DiagnosticContinuation::Unknown,
        extensions: rift_protocol::read::Extensions(std::collections::BTreeMap::new()),
        language: None,
    }
}

/// The longest prefix of `text` within `bytes_max` that ends on a character
/// boundary. The walk back is bounded by UTF-8 itself: at most three steps.
pub(crate) fn bounded_prefix(text: &str, bytes_max: usize) -> &str {
    if text.len() <= bytes_max {
        return text;
    }
    let mut end = bytes_max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Finding carried when follow-up snapshot cannot rebuild. Current-tree
/// reads refuse that dirty epoch until one can.
pub(crate) fn stale_snapshot_diagnostic(error: &ReadError) -> rift_protocol::read::Diagnostic {
    rift_protocol::read::Diagnostic {
        severity: rift_protocol::read::Severity::Warning,
        code: Some(DiagnosticCode::SnapshotStale.code()),
        message: format!(
            "the change landed, and the read snapshot could not refresh; \
             current-tree reads wait for a successful workspace reindex: {error}"
        ),
        span: None,
        related: Vec::new(),
        tags: Vec::new(),
        reliability: rift_protocol::read::DiagnosticReliability::Reliable,
        continuation: rift_protocol::read::DiagnosticContinuation::Unknown,
        extensions: rift_protocol::read::Extensions(std::collections::BTreeMap::new()),
        language: None,
    }
}

/// Boundary view of a read failure: the projection a tool handler serves as
/// the JSON-RPC error object the design documents — code `-32000`, the
/// rendered failure line as `message`, and the typed [`wire::ErrorData`] as
/// `data`.
pub(crate) trait WireFailure {
    /// The JSON-RPC error object for this failure, naming the phase it
    /// stopped in.
    fn tool_error(&self, phase: wire::ErrorPhase) -> ErrorData;

    /// The typed wire payload for this failure.
    fn wire_error(&self, phase: wire::ErrorPhase) -> wire::ErrorData;

    /// The failure's source chain as bounded `causes` entries, outermost
    /// first. Each level inherits the outer classification, which the read
    /// error already resolved through the concrete failure it wraps.
    fn wire_causes(&self) -> Vec<wire::ErrorCause>;
}

impl<K: Fault> WireFailure for rift_core::Error<K> {
    fn tool_error(&self, phase: wire::ErrorPhase) -> ErrorData {
        let message = self.to_string();
        let data = serde_json::to_value(self.wire_error(phase)).ok();
        ErrorData::new(RIFT_ERROR_CODE, message, data)
    }

    fn wire_error(&self, phase: wire::ErrorPhase) -> wire::ErrorData {
        let descriptor = self.descriptor();
        wire::ErrorData {
            code: wire_code(descriptor.name()),
            message: self.to_string(),
            retry: descriptor.retry(),
            phase,
            diagnostics: Vec::new(),
            limit: self.fault().limit_evidence(),
            causes: self.wire_causes(),
        }
    }

    fn wire_causes(&self) -> Vec<wire::ErrorCause> {
        let descriptor = self.descriptor();
        bounded_causes(
            wire_code(descriptor.name()),
            descriptor.retry(),
            std::error::Error::source(self),
        )
    }
}

/// Walks one source chain into bounded `causes` entries, outermost first.
/// Every level inherits the classification and retry guidance passed in,
/// which the failure already resolved through the concrete fault it wraps.
pub(crate) fn bounded_causes(
    code: wire::ErrorCode,
    retry: wire::RetryDirective,
    outermost: Option<&(dyn std::error::Error + 'static)>,
) -> Vec<wire::ErrorCause> {
    let mut causes = Vec::new();
    let mut source = outermost;
    while let Some(current) = source {
        if causes.len() == ERROR_CAUSES_MAX {
            break;
        }
        causes.push(wire::ErrorCause {
            code,
            message: current.to_string(),
            retry,
        });
        source = current.source();
    }
    causes
}

/// The wire code for one registry identity. The registry composes the wire
/// enum, so this is a projection, not a mapping; a CLI-only identity never
/// reaches this boundary, and classifies as `internal_error` if one does.
pub(crate) fn wire_code(name: ErrorName) -> wire::ErrorCode {
    match name {
        ErrorName::Wire(code) => code,
        ErrorName::Cli(_) => wire::ErrorCode::InternalError,
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use rift_core::{CliCode, ErrorName, SourceVisibility};
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::error as wire;
    use rift_server::{ReadFault, ReadService};

    use super::WireFailure;

    #[test]
    fn cli_identity_projects_to_internal_error_on_the_wire() {
        assert_eq!(
            super::wire_code(ErrorName::Cli(CliCode::ArtifactStale)),
            wire::ErrorCode::InternalError
        );
    }

    #[derive(Debug)]
    struct Link {
        depth: usize,
        inner: Option<Box<Link>>,
    }

    impl std::fmt::Display for Link {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "link {}", self.depth)
        }
    }

    impl Error for Link {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.inner
                .as_deref()
                .map(|link| link as &(dyn Error + 'static))
        }
    }

    #[test]
    fn cause_walk_stops_at_the_declared_bound() {
        let mut chained = Link {
            depth: 0,
            inner: None,
        };
        for depth in 1..=super::ERROR_CAUSES_MAX + 2 {
            chained = Link {
                depth,
                inner: Some(Box::new(chained)),
            };
        }
        let causes = super::bounded_causes(
            wire::ErrorCode::StorageFailure,
            wire::RetryDirective::Never,
            Some(&chained),
        );
        assert_eq!(
            causes.len(),
            super::ERROR_CAUSES_MAX,
            "a chain deeper than the bound must truncate at the bound"
        );
    }

    fn probe_hook() -> rift_protocol::configuration::CommandHook {
        use rift_protocol::configuration::{ChangedPaths, Determinism, HookKind, HookType};
        rift_protocol::configuration::CommandHook {
            r#type: HookType::Command,
            id: "tests".to_owned(),
            kind: HookKind::Test,
            program: "cargo".to_owned(),
            arguments: vec!["test".to_owned()],
            changed_paths: ChangedPaths::None,
            working_directory: rift_protocol::read::ProjectPath(String::new()),
            environment: std::collections::BTreeMap::new(),
            timeout: rift_protocol::configuration::Duration::from_millis(120_000),
            output_limit: rift_protocol::configuration::ByteSize::from_bytes(4_096),
            guarantees: Vec::new(),
            determinism: Determinism::Deterministic,
        }
    }

    fn silent_run(status: rift_server::HookStatus, exit_code: Option<i32>) -> rift_server::HookRun {
        rift_server::HookRun {
            id: "tests".to_owned(),
            status,
            exit_code,
            stdout: rift_server::CapturedStream::default(),
            stderr: rift_server::CapturedStream::default(),
        }
    }

    #[test]
    fn failed_hook_finding_quotes_exit_code_and_nonempty_streams() {
        use rift_server::{CapturedStream, HookStatus};
        let mut run = silent_run(HookStatus::Failed, Some(1));
        run.stdout = CapturedStream {
            text: "boom".to_owned(),
            captured_bytes: 4,
            total_bytes: 4,
            truncated: false,
        };
        let finding = super::hook_failure_diagnostic(&probe_hook(), &run);
        assert_eq!(finding.severity, rift_protocol::read::Severity::Error);
        assert_eq!(finding.code.as_deref(), Some("rift.hook.failed"));
        assert!(
            finding.message.contains("exited 1")
                && finding.message.contains("stdout (4 of 4 bytes): boom")
                && !finding.message.contains("stderr"),
            "{}",
            finding.message
        );
    }

    #[test]
    #[should_panic(expected = "a passing hook contributes guarantees, not findings")]
    fn passing_hook_finding_is_a_programmer_error() {
        let run = silent_run(rift_server::HookStatus::Passed, Some(0));
        let _ = super::hook_failure_diagnostic(&probe_hook(), &run);
    }

    #[test]
    fn hook_finding_accounts_for_every_non_passing_outcome() {
        use rift_server::HookStatus;
        let cases = [
            (HookStatus::Failed, None, "exited nonzero"),
            (HookStatus::TimedOut, None, "killed after 120000ms"),
            (
                HookStatus::Error("failed to launch: missing".to_owned()),
                None,
                "failed to launch: missing",
            ),
        ];
        for (status, exit_code, expected) in cases {
            let finding =
                super::hook_failure_diagnostic(&probe_hook(), &silent_run(status, exit_code));
            assert!(
                finding.message.contains(expected),
                "{expected} missing from {}",
                finding.message
            );
        }
    }

    #[test]
    fn bounded_prefix_cuts_on_character_boundaries() {
        assert_eq!(super::bounded_prefix("short", 16), "short");
        assert_eq!(super::bounded_prefix("ééé", 3), "é");
        assert_eq!(super::bounded_prefix("ééé", 4), "éé");
    }

    #[test]
    fn stale_snapshot_finding_carries_its_code_and_the_render() {
        let error = rift_server::ReadError::from(ReadFault::Unsupported {
            capability: "probe",
        });
        let finding = super::stale_snapshot_diagnostic(&error);
        assert_eq!(finding.code.as_deref(), Some("rift.snapshot.stale"));
        assert_eq!(finding.severity, rift_protocol::read::Severity::Warning);
        assert!(
            finding.message.contains("the change landed"),
            "{}",
            finding.message
        );
    }

    #[test]
    fn wire_causes_walk_the_source_chain_with_inherited_classification() {
        let error = ReadService::build(
            std::path::Path::new("not-a-real-rift-workspace"),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )
        .expect_err("missing root must fail");
        let causes = error.wire_causes();
        assert!(!causes.is_empty(), "sourced failure must yield causes");
        assert!(causes.len() <= super::ERROR_CAUSES_MAX);
        let code = super::wire_code(error.descriptor().name());
        for cause in &causes {
            assert!(!cause.message.is_empty(), "cause message must be rendered");
            assert_eq!(cause.code, code);
        }
    }
}
