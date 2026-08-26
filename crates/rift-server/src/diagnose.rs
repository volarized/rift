//! Engine diagnostics pulled over the paths one applied change touched.
//!
//! After a change lands, each changed path whose language has an engine
//! advertising diagnostic pulls is notified through the engine's own
//! watched-file registration, opened with its published bytes, pulled, and
//! closed; the engine's findings ride the change summary, mapped and
//! bounded. The change already applied, so nothing here refuses or fails
//! the call: an engine failure degrades to one warning naming the engine,
//! and an engine without the capability stays silent.
//!
//! The engine slot absorbs every transient condition first - a refusal the
//! engine invites again, an engine still analyzing, an engine that died,
//! an engine that reported nothing before it had announced any work of its
//! own - under that engine's `[engines.<name>.retry]` and
//! `[engines.<name>.restart]` tables. What reaches this module is either
//! the engine's settled findings or the condition that outlasted the whole
//! budget. An engine still analyzing on every attempt, and one whose pull
//! answered empty while its own readiness stayed unconfirmed, both
//! degrade to `rift.engine.unready`: an empty list would otherwise read as
//! clean bytes, and a settled-looking answer from an engine that has never
//! proven it is settled is not evidence of anything.

use std::collections::{BTreeMap, BTreeSet};

use lsp_types::request::{DocumentDiagnosticRequest, Request as _};
use lsp_types::{Diagnostic as LspDiagnostic, DiagnosticSeverity, NumberOrString};
use rift_core::ProjectPath as CoreProjectPath;
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::position::LineIndex;
use rift_lsp::session::{EngineError, EngineFault, EngineReadiness, EngineSession};
use rift_protocol::read::{
    Diagnostic, DiagnosticCode, DiagnosticContinuation, DiagnosticReliability, Extensions, FileId,
    Language, ProjectPath, Severity, SourceSpan, TextRange,
};

use crate::engine::{EnginePool, EngineSlot};
use crate::read::{ReadService, file_id};

/// Most mapped engine findings one applied change carries.
pub const ENGINE_DIAGNOSTICS_PER_CHANGE_MAX: usize = 16;

/// Most paths one `rift.engine.unready` warning names.
const ENGINE_UNREADY_PATHS_MAX: usize = 16;

/// Pulls engine diagnostics for every changed path an engine serves.
///
/// `reads` is the snapshot published after the change, so each document
/// opens with exactly the bytes the change landed; a path the snapshot no
/// longer holds - one the change deleted - is skipped. Findings are
/// bounded by [`ENGINE_DIAGNOSTICS_PER_CHANGE_MAX`] across all paths. An
/// engine that fails contributes one warning naming it and is not asked
/// again within this walk; an engine without the pull capability
/// contributes nothing. The slot has already spent that engine's whole
/// retry budget on the path before any warning is raised.
///
/// A path whose pull could not be proven settled - the engine exhausted
/// every attempt still analyzing, or answered empty while it had never
/// confirmed its own readiness - is held out of the mapped findings and
/// named instead in one `rift.engine.unready` warning per engine and
/// language, grouping every such path from this walk.
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
    let mut unready: BTreeMap<(String, String), (Language, Vec<ProjectPath>)> = BTreeMap::new();
    for path in paths {
        if findings.len() >= ENGINE_DIAGNOSTICS_PER_CHANGE_MAX {
            break;
        }
        let Ok(core_path) = CoreProjectPath::new(path.0.as_str()) else {
            continue;
        };
        let Some(file) = reads.index().file(&core_path) else {
            continue;
        };
        let language = file.syntax().language().clone();
        let Some(slot) = engines.engine_for(&language) else {
            continue;
        };
        if ended_engines.contains(slot.name()) {
            continue;
        }
        match pulled_diagnostics(slot, &core_path, &language, file.source()).await {
            Ok((items, _encoding, readiness))
                if items.is_empty() && readiness == EngineReadiness::Unconfirmed =>
            {
                unready_paths(&mut unready, slot.name(), &language).push(path.clone());
            }
            Ok((items, encoding, _readiness)) => {
                let unit = file_id(&core_path);
                let index = LineIndex::new(file.source());
                let remaining = ENGINE_DIAGNOSTICS_PER_CHANGE_MAX - findings.len();
                findings.extend(
                    items
                        .iter()
                        .take(remaining)
                        .map(|item| mapped_diagnostic(item, &unit, &index, encoding, &language)),
                );
            }
            Err(error) if matches!(error.fault(), EngineFault::Analyzing { .. }) => {
                unready_paths(&mut unready, slot.name(), &language).push(path.clone());
            }
            Err(error) => {
                if !matches!(error.fault(), EngineFault::CapabilityAbsent { .. }) {
                    findings.push(engine_warning(slot.name(), &error));
                }
                // Silence for an absent capability, one warning otherwise;
                // either way this engine is not asked again in this walk.
                ended_engines.insert(slot.name().to_owned());
            }
        }
    }
    for ((engine, _identity_segment), (language, unready_for)) in unready {
        if findings.len() >= ENGINE_DIAGNOSTICS_PER_CHANGE_MAX {
            break;
        }
        findings.push(unready_warning(&engine, &language, &unready_for));
    }
    findings
}

/// The paths already recorded as unready for one engine and language,
/// inserting an empty record on first use.
fn unready_paths<'record>(
    record: &'record mut BTreeMap<(String, String), (Language, Vec<ProjectPath>)>,
    engine: &str,
    language: &Language,
) -> &'record mut Vec<ProjectPath> {
    &mut record
        .entry((engine.to_owned(), language.identity_segment()))
        .or_insert_with(|| (language.clone(), Vec::new()))
        .1
}

/// One open-pull-close conversation on the claimed engine's slot.
async fn pulled_diagnostics(
    slot: &EngineSlot,
    path: &CoreProjectPath,
    language: &Language,
    source: &str,
) -> Result<(Vec<LspDiagnostic>, PositionEncoding, EngineReadiness), EngineError> {
    // The boxed future may only borrow the session, so each attempt gets
    // its own owned copy of the request data.
    let request_path = path.clone();
    let request_language = language.name.clone();
    let request_source = source.to_owned();
    slot.request(move |session: &mut EngineSession| {
        let path = request_path.clone();
        let language = request_language.clone();
        let text = request_source.clone();
        Box::pin(async move { pull_on_session(session, &path, &language, text).await })
    })
    .await
}

/// Notifies, opens, pulls, and closes one document on a running session.
///
/// The capability gate runs before the notification and the open, so an
/// engine without pulls is never handed one. The notification records
/// that this path changed on disk before the pull is sent, so the pull
/// starts only once the announced work that notification triggers has
/// been read - the outer slot resends the whole attempt while the session
/// still reads as analyzing, so a progress cycle interleaved before the
/// pull's own answer is not missed. A failed pull still attempts the
/// close; a session the fault already ended refuses it, which changes
/// nothing.
async fn pull_on_session(
    session: &mut EngineSession,
    path: &CoreProjectPath,
    language_id: &str,
    text: String,
) -> Result<(Vec<LspDiagnostic>, PositionEncoding, EngineReadiness), EngineError> {
    if !session.capabilities().pull_diagnostics {
        return Err(rift_core::Error::new(EngineFault::CapabilityAbsent {
            capability: DocumentDiagnosticRequest::METHOD.to_owned(),
        }));
    }
    let encoding = session.capabilities().position_encoding;
    let notified_paths = std::slice::from_ref(path);
    session.notify_changed_paths(notified_paths).await?;
    session.open(path, language_id, text).await?;
    let pulled = session.pull_diagnostics(path).await;
    let readiness = session.readiness();
    // The close is best-effort: a session the pull's fault ended refuses
    // it, and the pull's own outcome is what the caller acts on.
    let _ = session.close(path).await;
    Ok((pulled?, encoding, readiness))
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
///
/// Every failure that reaches here is one the retry budget could not
/// absorb: a broken exchange, a dead engine the restart budget refused to
/// replace, a timeout. An engine still analyzing on every attempt, or one
/// whose empty answer never confirmed its own readiness, is not reported
/// through this warning at all - see [`unready_warning`].
fn engine_warning(engine: &str, error: &EngineError) -> Diagnostic {
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

/// The one warning naming every changed path whose post-apply diagnostics
/// from `engine`, for `language`, could not be proven settled.
///
/// Grouped one warning per engine and language rather than one per path:
/// the reason is the same for every path in the group, and the group is
/// bounded by [`ENGINE_UNREADY_PATHS_MAX`].
fn unready_warning(engine: &str, language: &Language, paths: &[ProjectPath]) -> Diagnostic {
    let named = paths
        .iter()
        .take(ENGINE_UNREADY_PATHS_MAX)
        .map(|path| path.0.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Diagnostic {
        severity: Severity::Warning,
        code: Some(DiagnosticCode::EngineUnready.code()),
        message: format!(
            "engine {engine} for language {} never confirmed its own readiness, so its \
             diagnostics for {named} are not proven settled",
            language.identity_segment()
        ),
        span: None,
        related: Vec::new(),
        tags: Vec::new(),
        reliability: DiagnosticReliability::Reliable,
        continuation: DiagnosticContinuation::Unknown,
        extensions: Extensions(BTreeMap::new()),
        language: Some(language.clone()),
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
    fn engine_warnings_carry_their_own_code_and_name_the_engine() {
        let failed = engine_warning("fake", &rift_core::Error::new(EngineFault::Ended));
        assert_eq!(failed.severity, Severity::Warning);
        assert_eq!(failed.code.as_deref(), Some("rift.engine.failed"));
        assert!(failed.message.contains("fake"), "{}", failed.message);
    }

    #[test]
    fn unready_warnings_carry_their_own_code_and_name_every_path() {
        let language = Language {
            name: "rust".to_owned(),
            dialect: None,
        };
        let paths = [
            ProjectPath("lib.rs".to_owned()),
            ProjectPath("caller.rs".to_owned()),
        ];
        let warning = unready_warning("fake", &language, &paths);
        assert_eq!(warning.severity, Severity::Warning);
        assert_eq!(warning.code.as_deref(), Some("rift.engine.unready"));
        assert_eq!(warning.language, Some(language));
        assert!(warning.message.contains("fake"), "{}", warning.message);
        assert!(warning.message.contains("rust"), "{}", warning.message);
        assert!(warning.message.contains("lib.rs"), "{}", warning.message);
        assert!(warning.message.contains("caller.rs"), "{}", warning.message);
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

    /// One framed JSON-RPC message.
    fn framed(body: &str) -> String {
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    /// A workspace served by a canned `sh` engine that advertises pull
    /// diagnostics and never announces any work of its own: every
    /// `textDocument/diagnostic` pull it answers - the first, and the one
    /// resend an unconfirmed engine earns - carries no item. The script
    /// never reads its stdin; it writes both answers regardless of what
    /// the session sends.
    fn workspace_with_unconfirmed_engine(
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, ReadService, EnginePool) {
        let directory = tempfile::tempdir().expect("fixture directory");
        for (name, source) in files {
            std::fs::write(directory.path().join(name), source).expect("fixture file writes");
        }
        let reads = ReadService::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )
        .expect("fixture workspace indexes");
        let capabilities = framed(
            r#"{"jsonrpc":"2.0","id":0,"result":{"capabilities":{"diagnosticProvider":{"identifier":"fake","interFileDependencies":false,"workspaceDiagnostics":false}}}}"#,
        );
        let empty_pull = |id: u64| {
            framed(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"kind":"full","items":[]}}}}"#
            ))
        };
        let script = format!(
            "printf '%s' '{capabilities}{}{}'; sleep 0.2",
            empty_pull(1),
            empty_pull(2),
        );
        let engine = rift_protocol::configuration::EngineConfiguration {
            program: "sh".to_owned(),
            arguments: vec!["-c".to_owned(), script],
            environment: BTreeMap::new(),
            languages: vec!["rust".to_owned()],
            initialization_options: None,
            startup_timeout: rift_protocol::configuration::Duration::from_millis(10_000),
            request_timeout: rift_protocol::configuration::Duration::from_millis(10_000),
            output_limit: rift_protocol::configuration::ByteSize::from_bytes(4_096),
            retry: rift_protocol::retry::RetryPolicy {
                attempts: 2,
                delay: rift_protocol::configuration::Duration::from_millis(1),
                delay_limit: rift_protocol::configuration::Duration::from_millis(1),
            },
            restart: rift_protocol::retry::RestartPolicy::default(),
        };
        let engines = EnginePool::new(
            directory.path(),
            BTreeMap::from([("fake".to_owned(), engine)]),
        );
        (directory, reads, engines)
    }

    /// Reproduces the defect an unread readiness left behind: an
    /// unconfirmed engine's settled-looking empty pull - the one this
    /// session already spent its one resend on - used to read as clean
    /// bytes. It must instead surface as `rift.engine.unready`, naming
    /// the engine and the path, rather than silently reporting nothing.
    #[tokio::test]
    async fn an_unconfirmed_engines_settled_looking_empty_pull_is_reported_unready() {
        let (directory, reads, engines) =
            workspace_with_unconfirmed_engine(&[("lib.rs", "pub fn beacon() {}\n")]);
        let paths = vec![ProjectPath("lib.rs".to_owned())];
        let findings = engine_change_diagnostics(&engines, &reads, &paths).await;
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].code.as_deref(), Some("rift.engine.unready"));
        assert!(
            findings[0].message.contains("fake"),
            "{}",
            findings[0].message
        );
        assert!(
            findings[0].message.contains("lib.rs"),
            "{}",
            findings[0].message
        );
        engines.shutdown().await;
        drop(directory);
    }

    /// A workspace served by a canned `sh` engine that announces one
    /// work-done progress token and never ends it, and answers exactly one
    /// `textDocument/diagnostic` pull with no item while it is outstanding.
    /// With `retry.attempts` at 1 the slot exhausts its budget on that
    /// single attempt, so `pulled_diagnostics` surfaces
    /// `EngineFault::Analyzing` rather than a settled answer. The script
    /// never reads its stdin; it writes the fixed sequence regardless of
    /// what the session sends.
    fn workspace_with_an_engine_still_analyzing(
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, ReadService, EnginePool) {
        let directory = tempfile::tempdir().expect("fixture directory");
        for (name, source) in files {
            std::fs::write(directory.path().join(name), source).expect("fixture file writes");
        }
        let reads = ReadService::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )
        .expect("fixture workspace indexes");
        let capabilities = framed(
            r#"{"jsonrpc":"2.0","id":0,"result":{"capabilities":{"diagnosticProvider":{"identifier":"fake","interFileDependencies":false,"workspaceDiagnostics":false}}}}"#,
        );
        let progress_begin = framed(
            r#"{"jsonrpc":"2.0","method":"$/progress","params":{"token":"warm","value":{"kind":"begin","title":"loading"}}}"#,
        );
        let empty_pull = framed(r#"{"jsonrpc":"2.0","id":1,"result":{"kind":"full","items":[]}}"#);
        let script = format!("printf '%s' '{capabilities}{progress_begin}{empty_pull}'; sleep 0.2");
        let engine = rift_protocol::configuration::EngineConfiguration {
            program: "sh".to_owned(),
            arguments: vec!["-c".to_owned(), script],
            environment: BTreeMap::new(),
            languages: vec!["rust".to_owned()],
            initialization_options: None,
            startup_timeout: rift_protocol::configuration::Duration::from_millis(10_000),
            request_timeout: rift_protocol::configuration::Duration::from_millis(10_000),
            output_limit: rift_protocol::configuration::ByteSize::from_bytes(4_096),
            retry: rift_protocol::retry::RetryPolicy {
                attempts: 1,
                delay: rift_protocol::configuration::Duration::from_millis(1),
                delay_limit: rift_protocol::configuration::Duration::from_millis(1),
            },
            restart: rift_protocol::retry::RestartPolicy::default(),
        };
        let engines = EnginePool::new(
            directory.path(),
            BTreeMap::from([("fake".to_owned(), engine)]),
        );
        (directory, reads, engines)
    }

    /// An engine still analyzing on every attempt reports the same
    /// `rift.engine.unready` warning an unconfirmed engine's settled-
    /// looking empty answer does: both are answers the retry budget could
    /// not turn into a settled verdict, and neither is evidence the
    /// changed bytes are clean.
    #[tokio::test]
    async fn an_engine_still_analyzing_on_every_attempt_is_reported_unready() {
        let (directory, reads, engines) =
            workspace_with_an_engine_still_analyzing(&[("lib.rs", "pub fn beacon() {}\n")]);
        let paths = vec![ProjectPath("lib.rs".to_owned())];
        let findings = engine_change_diagnostics(&engines, &reads, &paths).await;
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].code.as_deref(), Some("rift.engine.unready"));
        assert!(
            findings[0].message.contains("fake"),
            "{}",
            findings[0].message
        );
        assert!(
            findings[0].message.contains("lib.rs"),
            "{}",
            findings[0].message
        );
        engines.shutdown().await;
        drop(directory);
    }

    /// A workspace served by a canned `sh` engine whose first path never
    /// confirms readiness - two empty pulls, exactly
    /// [`workspace_with_unconfirmed_engine`]'s script - and whose second
    /// path answers with [`ENGINE_DIAGNOSTICS_PER_CHANGE_MAX`] mapped
    /// diagnostics, filling the finding cap before the walk ever appends
    /// the first path's queued unready warning.
    fn workspace_with_an_unconfirmed_path_and_a_flooding_path(
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, ReadService, EnginePool) {
        let directory = tempfile::tempdir().expect("fixture directory");
        for (name, source) in files {
            std::fs::write(directory.path().join(name), source).expect("fixture file writes");
        }
        let reads = ReadService::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            rift_protocol::configuration::HistoryConfiguration::default(),
        )
        .expect("fixture workspace indexes");
        let capabilities = framed(
            r#"{"jsonrpc":"2.0","id":0,"result":{"capabilities":{"diagnosticProvider":{"identifier":"fake","interFileDependencies":false,"workspaceDiagnostics":false}}}}"#,
        );
        let empty_pull = |id: u64| {
            framed(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"kind":"full","items":[]}}}}"#
            ))
        };
        let items = (0..ENGINE_DIAGNOSTICS_PER_CHANGE_MAX)
            .map(|index| {
                format!(
                    r#"{{"range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":1}}}},"message":"flood-{index}"}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let flooding_pull = framed(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"result":{{"kind":"full","items":[{items}]}}}}"#
        ));
        let script = format!(
            "printf '%s' '{capabilities}{}{}{flooding_pull}'; sleep 0.2",
            empty_pull(1),
            empty_pull(2),
        );
        let engine = rift_protocol::configuration::EngineConfiguration {
            program: "sh".to_owned(),
            arguments: vec!["-c".to_owned(), script],
            environment: BTreeMap::new(),
            languages: vec!["rust".to_owned()],
            initialization_options: None,
            startup_timeout: rift_protocol::configuration::Duration::from_millis(10_000),
            request_timeout: rift_protocol::configuration::Duration::from_millis(10_000),
            output_limit: rift_protocol::configuration::ByteSize::from_bytes(8_192),
            retry: rift_protocol::retry::RetryPolicy {
                attempts: 2,
                delay: rift_protocol::configuration::Duration::from_millis(1),
                delay_limit: rift_protocol::configuration::Duration::from_millis(1),
            },
            restart: rift_protocol::retry::RestartPolicy::default(),
        };
        let engines = EnginePool::new(
            directory.path(),
            BTreeMap::from([("fake".to_owned(), engine)]),
        );
        (directory, reads, engines)
    }

    /// Once one path's mapped findings fill the cap, a still-queued
    /// unready warning from an earlier path in the same walk is never
    /// appended: the cap applies to the unready warnings the same way it
    /// applies to mapped findings, so the walk's total never exceeds it.
    #[tokio::test]
    async fn a_finding_cap_reached_before_the_unready_group_drops_the_queued_warning() {
        let (directory, reads, engines) =
            workspace_with_an_unconfirmed_path_and_a_flooding_path(&[
                ("a.rs", "pub fn a() {}\n"),
                ("b.rs", "pub fn b() {}\n"),
            ]);
        let paths = vec![
            ProjectPath("a.rs".to_owned()),
            ProjectPath("b.rs".to_owned()),
        ];
        let findings = engine_change_diagnostics(&engines, &reads, &paths).await;
        assert_eq!(
            findings.len(),
            ENGINE_DIAGNOSTICS_PER_CHANGE_MAX,
            "{findings:#?}"
        );
        assert!(
            findings
                .iter()
                .all(|finding| finding.code.as_deref() != Some("rift.engine.unready")),
            "the cap is reached before path a's queued unready warning is appended: \
             {findings:#?}"
        );
        engines.shutdown().await;
        drop(directory);
    }
}
