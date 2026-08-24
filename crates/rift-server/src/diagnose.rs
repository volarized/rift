//! Engine diagnostics pulled over the paths one applied change touched.
//!
//! After a change lands, each changed path whose language has an engine
//! advertising diagnostic pulls is opened with its published bytes, pulled,
//! and closed; the engine's findings ride the change summary, mapped and
//! bounded. The change already applied, so nothing here refuses or fails
//! the call: an engine failure degrades to one warning naming the engine,
//! and an engine without the capability stays silent. A refusal the engine
//! invites again - it cancelled the pull, or the content moved under it -
//! is resent under the engine's own `[engines.<name>.retry]` policy before
//! it degrades.

use std::collections::{BTreeMap, BTreeSet};

use lsp_types::request::{DocumentDiagnosticRequest, Request as _};
use lsp_types::{Diagnostic as LspDiagnostic, DiagnosticSeverity, NumberOrString};
use rift_core::ProjectPath as CoreProjectPath;
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::position::LineIndex;
use rift_lsp::session::{EngineError, EngineFault, EngineSession};
use rift_protocol::read::{
    Diagnostic, DiagnosticCode, DiagnosticContinuation, DiagnosticReliability, Extensions, FileId,
    Language, ProjectPath, Severity, SourceSpan, TextRange,
};
use rift_protocol::retry::RetryPolicy;

use crate::engine::{EnginePool, EngineSlot};
use crate::read::{ReadService, file_id};

/// Most mapped engine findings one applied change carries.
pub const ENGINE_DIAGNOSTICS_PER_CHANGE_MAX: usize = 16;

/// Pulls engine diagnostics for every changed path an engine serves.
///
/// `reads` is the snapshot published after the change, so each document
/// opens with exactly the bytes the change landed; a path the snapshot no
/// longer holds - one the change deleted - is skipped. Findings are
/// bounded by [`ENGINE_DIAGNOSTICS_PER_CHANGE_MAX`] across all paths. An
/// engine that fails contributes one warning naming it and is not asked
/// again within this walk; an engine without the pull capability
/// contributes nothing. Each path's pull is resent while the engine keeps
/// refusing it retryably and its `retry` table still allows an attempt, so
/// the walk asks at most `retry.attempts` times per path before degrading.
///
/// # Cancel safety
///
/// Dropping the future leaves at most one engine request pending; the
/// session discards its stale response on the next call.
pub async fn engine_change_diagnostics(
    engines: &EnginePool,
    reads: &ReadService,
    paths: &[ProjectPath],
) -> Vec<Diagnostic> {
    let mut findings = Vec::new();
    let mut ended_engines: BTreeSet<String> = BTreeSet::new();
    for path in paths {
        if findings.len() >= ENGINE_DIAGNOSTICS_PER_CHANGE_MAX {
            break;
        }
        let Ok(path) = CoreProjectPath::new(path.0.as_str()) else {
            continue;
        };
        let Some(file) = reads.index().file(&path) else {
            continue;
        };
        let language = file.syntax().language().clone();
        let Some(slot) = engines.engine_for(&language) else {
            continue;
        };
        if ended_engines.contains(slot.name()) {
            continue;
        }
        match pulled_diagnostics(slot, &path, &language, file.source()).await {
            Ok((items, encoding)) => {
                let unit = file_id(&path);
                let index = LineIndex::new(file.source());
                let remaining = ENGINE_DIAGNOSTICS_PER_CHANGE_MAX - findings.len();
                findings.extend(
                    items
                        .iter()
                        .take(remaining)
                        .map(|item| mapped_diagnostic(item, &unit, &index, encoding, &language)),
                );
            }
            Err(error) => {
                if !matches!(error.fault(), EngineFault::CapabilityAbsent { .. }) {
                    findings.push(engine_failure_diagnostic(slot.name(), &error));
                }
                // Silence for an absent capability, one warning otherwise;
                // either way this engine is not asked again in this walk.
                ended_engines.insert(slot.name().to_owned());
            }
        }
    }
    findings
}

/// One open-pull-close conversation on the claimed engine's slot.
async fn pulled_diagnostics(
    slot: &EngineSlot,
    path: &CoreProjectPath,
    language: &Language,
    source: &str,
) -> Result<(Vec<LspDiagnostic>, PositionEncoding), EngineError> {
    // The boxed future may only borrow the session, so each attempt gets
    // its own owned copy of the request data.
    let request_path = path.clone();
    let request_language = language.name.clone();
    let request_source = source.to_owned();
    let retry = slot.configuration().retry;
    slot.request(move |session: &mut EngineSession| {
        let path = request_path.clone();
        let language = request_language.clone();
        let text = request_source.clone();
        Box::pin(async move { pull_on_session(session, &path, &language, text, retry).await })
    })
    .await
}

/// Opens, pulls, and closes one document on a running session.
///
/// The capability gate runs before the open, so an engine without pulls is
/// never handed a document. A failed pull still attempts the close; a
/// session the fault already ended refuses it, which changes nothing.
async fn pull_on_session(
    session: &mut EngineSession,
    path: &CoreProjectPath,
    language_id: &str,
    text: String,
    retry: RetryPolicy,
) -> Result<(Vec<LspDiagnostic>, PositionEncoding), EngineError> {
    if !session.capabilities().pull_diagnostics {
        return Err(rift_core::Error::new(EngineFault::CapabilityAbsent {
            capability: DocumentDiagnosticRequest::METHOD.to_owned(),
        }));
    }
    let encoding = session.capabilities().position_encoding;
    session.open(path, language_id, text).await?;
    let pulled = pulled_within_attempts(session, path, retry).await;
    // The close is best-effort: a session the pull's fault ended refuses
    // it, and the pull's own outcome is what the caller acts on.
    let _ = session.close(path).await;
    Ok((pulled?, encoding))
}

/// Pulls one open document's diagnostics, resending a retryable refusal.
///
/// The document stays open across the attempts, so no reopen can cancel
/// the next pull. The loop runs at most `retry.attempts` times and waits
/// what the policy answers between them: a growing wait that starts at
/// `retry.delay` and is held at `retry.delay_limit`. The change already
/// applied, so those waits only delay its answer; at the shipped defaults
/// they add at most 9.75s. A refusal that is not retryable comes back at
/// once, and a retryable one that outlasts the attempt bound comes back as
/// itself, for the caller to degrade into its single warning.
async fn pulled_within_attempts(
    session: &mut EngineSession,
    path: &CoreProjectPath,
    retry: RetryPolicy,
) -> Result<Vec<LspDiagnostic>, EngineError> {
    let mut attempt: u64 = 1;
    loop {
        let refusal = match session.pull_diagnostics(path).await {
            Ok(items) => return Ok(items),
            Err(refusal) => refusal,
        };
        if !refusal.fault().is_retryable_refusal() {
            return Err(refusal);
        }
        let Some(wait) = retry.delay_after(attempt) else {
            return Err(refusal);
        };
        tokio::time::sleep(wait).await;
        attempt += 1;
    }
}

/// One LSP diagnostic mapped into the change summary's carrier.
///
/// The message and a string code carry over; a numeric code has no wire
/// place and is dropped. The range converts to a byte span through the
/// published bytes' line index, and a position that does not land in them
/// drops the span, never the finding. Related entries and tags are not
/// mapped. Reliability is `reliable` - the engine analyzed the exact
/// published bytes - and continuation stays `unknown`, because LSP does
/// not say whether a finding is an artefact of source that stops mid-way.
fn mapped_diagnostic(
    item: &LspDiagnostic,
    unit: &FileId,
    index: &LineIndex<'_>,
    encoding: PositionEncoding,
    language: &Language,
) -> Diagnostic {
    let code = match &item.code {
        Some(NumberOrString::String(code)) => Some(code.clone()),
        _ => None,
    };
    let start = index.byte_offset(encoding, item.range.start);
    let end = index.byte_offset(encoding, item.range.end);
    let span = match (start, end) {
        (Ok(start), Ok(end)) if start <= end => Some(SourceSpan {
            unit: unit.clone(),
            range: TextRange {
                start: start as u64,
                end: end as u64,
            },
        }),
        _ => None,
    };
    Diagnostic {
        severity: mapped_severity(item.severity),
        code,
        message: item.message.clone(),
        span,
        related: Vec::new(),
        tags: Vec::new(),
        reliability: DiagnosticReliability::Reliable,
        continuation: DiagnosticContinuation::Unknown,
        extensions: Extensions(BTreeMap::new()),
        language: Some(language.clone()),
    }
}

/// The Rift severity one LSP severity maps to.
///
/// LSP leaves an absent severity to the client; the strictest reading -
/// error - keeps a real finding visible.
fn mapped_severity(severity: Option<DiagnosticSeverity>) -> Severity {
    match severity {
        Some(DiagnosticSeverity::WARNING) => Severity::Warning,
        Some(DiagnosticSeverity::INFORMATION) => Severity::Info,
        Some(DiagnosticSeverity::HINT) => Severity::Hint,
        Some(_) | None => Severity::Error,
    }
}

/// The one warning an engine that failed to serve diagnostics contributes.
fn engine_failure_diagnostic(engine: &str, error: &EngineError) -> Diagnostic {
    Diagnostic {
        severity: Severity::Warning,
        code: Some(DiagnosticCode::EngineFailed.code()),
        message: format!(
            "engine {engine} could not serve diagnostics over the applied change: {error}"
        ),
        span: None,
        related: Vec::new(),
        tags: Vec::new(),
        reliability: DiagnosticReliability::Reliable,
        continuation: DiagnosticContinuation::Unknown,
        extensions: Extensions(BTreeMap::new()),
        language: None,
    }
}

#[cfg(test)]
mod tests {
    use lsp_types::{Position, Range};

    use super::*;

    fn item(range: Range) -> LspDiagnostic {
        LspDiagnostic {
            range,
            message: "engine finding".to_owned(),
            ..LspDiagnostic::default()
        }
    }

    fn at(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn mapped(item: &LspDiagnostic, source: &str) -> Diagnostic {
        let unit = FileId("rift://file/lib.rs".to_owned());
        let index = LineIndex::new(source);
        let language = Language {
            name: "rust".to_owned(),
            dialect: None,
        };
        mapped_diagnostic(item, &unit, &index, PositionEncoding::Utf8, &language)
    }

    #[test]
    fn severities_map_and_an_absent_severity_reads_as_error() {
        let rows = [
            (Some(DiagnosticSeverity::ERROR), Severity::Error),
            (Some(DiagnosticSeverity::WARNING), Severity::Warning),
            (Some(DiagnosticSeverity::INFORMATION), Severity::Info),
            (Some(DiagnosticSeverity::HINT), Severity::Hint),
            (None, Severity::Error),
        ];
        for (lsp, expected) in rows {
            assert_eq!(mapped_severity(lsp), expected, "{lsp:?}");
        }
        let out_of_range: DiagnosticSeverity =
            serde_json::from_value(serde_json::json!(9)).expect("the newtype takes any integer");
        assert_eq!(
            mapped_severity(Some(out_of_range)),
            Severity::Error,
            "a severity outside the protocol's four reads as error"
        );
    }

    #[test]
    fn string_codes_carry_over_and_numeric_codes_drop() {
        let source = "pub fn beacon() {}\n";
        let mut coded = item(Range::default());
        coded.code = Some(NumberOrString::String("E0308".to_owned()));
        assert_eq!(mapped(&coded, source).code.as_deref(), Some("E0308"));
        coded.code = Some(NumberOrString::Number(7));
        assert_eq!(mapped(&coded, source).code, None);
    }

    #[test]
    fn ranges_convert_to_byte_spans_through_the_published_bytes() {
        let source = "pub fn beacon() {}\nlet x = 1;\n";
        let converted = mapped(
            &item(Range {
                start: at(1, 4),
                end: at(1, 5),
            }),
            source,
        );
        let span = converted.span.expect("the range lands in the source");
        assert_eq!(span.range, TextRange { start: 23, end: 24 });
        assert_eq!(span.unit.0, "rift://file/lib.rs");
        assert_eq!(
            converted.language,
            Some(Language {
                name: "rust".to_owned(),
                dialect: None
            })
        );
        assert_eq!(converted.reliability, DiagnosticReliability::Reliable);
        assert_eq!(converted.continuation, DiagnosticContinuation::Unknown);
    }

    #[test]
    fn a_range_outside_the_published_bytes_drops_the_span_not_the_finding() {
        let source = "short\n";
        let converted = mapped(
            &item(Range {
                start: at(9, 0),
                end: at(9, 5),
            }),
            source,
        );
        assert!(converted.span.is_none());
        assert_eq!(converted.message, "engine finding");
        let reversed = mapped(
            &item(Range {
                start: at(0, 4),
                end: at(0, 1),
            }),
            source,
        );
        assert!(
            reversed.span.is_none(),
            "a reversed range converts to no span"
        );
    }

    #[test]
    fn engine_failure_diagnostic_names_the_engine_and_the_code() {
        let error = rift_core::Error::new(EngineFault::Ended);
        let warning = engine_failure_diagnostic("fake", &error);
        assert_eq!(warning.severity, Severity::Warning);
        assert_eq!(warning.code.as_deref(), Some("rift.engine.failed"));
        assert!(warning.message.contains("fake"), "{}", warning.message);
    }

    #[tokio::test]
    async fn paths_without_an_engine_or_index_entry_contribute_nothing() {
        let directory = tempfile::tempdir().expect("fixture directory");
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")
            .expect("fixture file writes");
        let reads = ReadService::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )
        .expect("fixture workspace indexes");
        let engines = EnginePool::new(directory.path(), BTreeMap::new());
        let paths = vec![
            ProjectPath("lib.rs".to_owned()),
            ProjectPath("vanished.rs".to_owned()),
            ProjectPath("/absolute".to_owned()),
        ];
        let findings = engine_change_diagnostics(&engines, &reads, &paths).await;
        assert!(findings.is_empty());
    }
}
