//! File move through a configured language engine's will-rename request.
//!
//! The server moves any visible regular file and asks the language engine
//! for reference updates; the proposal compiles through the rename kernel
//! into whole-file rewrites, and the move and every rewrite land through
//! one atomic publish. Without a syntax provider claiming the file's
//! language, without an engine, without the will-rename capability, or
//! with filters that do not cover the file, the move still lands and the
//! result carries a warning that references were not updated. So does a
//! move whose engine answers with no edit at all, whatever its readiness
//! said before that answer: the contract already permits a move that needs
//! no reference rewritten, and the warning tells the caller which case
//! this was rather than staying silent about it.

use std::collections::BTreeMap;
use std::path::Path;

use lsp_types::{TextEdit, WorkspaceEdit};
use rift_core::ProjectPath as CoreProjectPath;
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::session::{
    EngineError, EngineFault, EngineReadiness, EngineSession, proposes_no_edit,
};
use rift_protocol::change::{
    ChangeResult, MoveFileParams, OperationPreconditionKind, PreconditionValue, RefusalReason,
};
use rift_protocol::read::{Diagnostic, DiagnosticCode, Severity};

use crate::engine::{EnginePool, EngineSlot};
use crate::read::{ReadError, ReadFault, ReadService};
use crate::rename::{
    PlanEnd, PlannedRewrite, ProposalContext, compiled_rewrites, failed_precondition,
    plan_diagnostic, proposal_documents, refused_oversized, workspace_tree_root,
};

/// The operation prose opening every move refusal detail.
const MOVE_OPERATION: &str = "file move";

/// One move, verified and compiled, not yet written. The change lane
/// re-proves every base against the disk before writing.
#[derive(Debug)]
pub struct MovePlan {
    pub(crate) from: CoreProjectPath,
    pub(crate) to: CoreProjectPath,
    /// The moved file's verified bytes: the compile base for edits the
    /// engine addressed to either the source or the destination path.
    pub(crate) moved_source: String,
    /// The bytes that land at the destination - the moved bytes, with any
    /// engine edits addressed to the moved file applied.
    pub(crate) moved_next: String,
    /// Reference rewrites in other files, compiled from the proposal.
    pub(crate) rewrites: Vec<PlannedRewrite>,
    /// Why the references were not updated, when they were not; the apply
    /// attaches it to the summary as its warning.
    pub(crate) references_not_updated: Option<ReferencesNotUpdated>,
}

/// What planning decided: a plan ready for the change lane, or the refusal
/// that ends the request with the targeted tree untouched.
#[derive(Debug)]
pub enum MoveResolution {
    /// The move verified; the change lane re-proves and writes it.
    Planned(MovePlan),
    /// Planning refused; the targeted tree is untouched.
    Refused(ChangeResult),
}

/// Why the moved file's references were not updated.
///
/// [`Self::NoLanguageClaimed`] names a file no shipped syntax provider parses, so no
/// language identity exists to ask an engine about. The next three name a known language
/// whose engine was never asked. The last two name one that was, and proposed no edit:
/// [`Self::AnsweredNothing`] when the engine had never confirmed its own readiness at the
/// time it answered, so nothing distinguishes that answer from the answer of an engine with
/// nothing to update; [`Self::NoReferenceEdits`] once it had, which is the engine's own
/// settled verdict that the move needs no reference rewrite.
#[derive(Debug)]
pub(crate) enum ReferencesNotUpdated {
    /// No shipped syntax provider parses the moved file, so its language is unknown and
    /// no engine could be asked about it.
    NoLanguageClaimed,
    /// No engine claims the moved file's language.
    NoEngine {
        /// The unserved language identity segment.
        language_segment: String,
    },
    /// The engine does not advertise `workspace/willRenameFiles`.
    CapabilityAbsent {
        /// The engine's configured name.
        engine: String,
    },
    /// The engine's will-rename filters do not cover the moved file.
    FilterMismatch {
        /// The engine's configured name.
        engine: String,
    },
    /// The engine proposed no edit while its own readiness stayed
    /// unconfirmed.
    AnsweredNothing {
        /// The engine's configured name.
        engine: String,
    },
    /// The engine proposed no edit after confirming its own readiness.
    NoReferenceEdits {
        /// The engine's configured name.
        engine: String,
    },
}

impl ReferencesNotUpdated {
    /// The reason one will-rename answer that proposed no edit is
    /// reported under, decided by the engine's readiness as of that
    /// answer.
    fn no_edit(engine: String, readiness: EngineReadiness) -> Self {
        if readiness == EngineReadiness::Unconfirmed {
            Self::AnsweredNothing { engine }
        } else {
            Self::NoReferenceEdits { engine }
        }
    }

    /// The warning an applied move carries, naming why its references
    /// were not updated.
    pub(crate) fn diagnostic(&self) -> Diagnostic {
        let reason = match self {
            Self::NoLanguageClaimed => "no syntax provider claims this file's language, so \
                                         no engine could be asked to update references"
                .to_owned(),
            Self::NoEngine { language_segment } => {
                format!("no engine is configured for language {language_segment}")
            }
            Self::CapabilityAbsent { engine } => {
                format!("engine {engine} does not advertise workspace/willRenameFiles")
            }
            Self::FilterMismatch { engine } => {
                format!("engine {engine}'s will-rename filters do not cover the moved file")
            }
            Self::AnsweredNothing { engine } => format!(
                "engine {engine} proposed none and has announced no work of its own, \
                 so it may not have read the file yet"
            ),
            Self::NoReferenceEdits { engine } => {
                format!("engine {engine} proposed no reference edits for the move")
            }
        };
        let mut diagnostic = plan_diagnostic(format!(
            "the file moved and its references were not updated: {reason}"
        ));
        diagnostic.severity = Severity::Warning;
        diagnostic.code = Some(DiagnosticCode::MoveReferencesNotUpdated.code());
        diagnostic
    }
}

/// Plans one file move: verifies both paths, asks the configured engine
/// for reference updates when it covers the file, and compiles the
/// proposal into whole-file rewrites for the change lane.
///
/// # Errors
///
/// Returns [`ReadError`] for an invalid request - a malformed path, or a
/// destination equal to the source - a filesystem failure, or an engine
/// that failed to serve. A request the server declines returns a refused
/// [`ChangeResult`] inside [`MoveResolution`] instead.
///
/// # Cancel safety
///
/// Dropping the future writes nothing; an engine request in flight is
/// discarded by the session, and the session stays in its slot.
pub async fn plan_move(
    reads: &ReadService,
    engines: &EnginePool,
    workspace_root: &Path,
    params: &MoveFileParams,
) -> Result<MoveResolution, ReadError> {
    match planned_move(reads, engines, workspace_root, params).await {
        Ok(plan) => Ok(MoveResolution::Planned(plan)),
        Err(PlanEnd::Refused(refusal)) => Ok(MoveResolution::Refused(refusal)),
        Err(PlanEnd::Failed(error)) => Err(error),
    }
}

/// The planning pipeline behind [`plan_move`], with every early end as one
/// typed [`PlanEnd`].
async fn planned_move(
    reads: &ReadService,
    engines: &EnginePool,
    workspace_root: &Path,
    params: &MoveFileParams,
) -> Result<MovePlan, PlanEnd> {
    let (from, to) = move_targets(params)?;
    let source = resolved_source(reads, workspace_root, &from).await?;
    refused_occupied_destination(workspace_root, &to).await?;
    refused_invisible_destination(reads, workspace_root, &to)?;
    refused_oversized(&from, source.text.len(), MOVE_OPERATION)?;
    let proposal = engine_proposal(
        engines,
        &from,
        &to,
        source.language_segment.as_deref(),
        &source.text,
    )
    .await?;
    match proposal {
        EngineProposal::Nothing(reason) => Ok(unedited_plan(from, to, source.text, Some(reason))),
        EngineProposal::Answered { edit, encoding } => {
            compiled_move(workspace_root, &edit, encoding, from, to, source.text).await
        }
    }
}

/// The moved file's language segment - present only when the syntax index claims the
/// path - and its bytes, read directly from disk.
struct MovedSource {
    language_segment: Option<String>,
    text: String,
}

/// A plan whose moved bytes land unchanged, with no reference rewrites.
fn unedited_plan(
    from: CoreProjectPath,
    to: CoreProjectPath,
    moved_source: String,
    references_not_updated: Option<ReferencesNotUpdated>,
) -> MovePlan {
    MovePlan {
        from,
        to,
        moved_next: moved_source.clone(),
        moved_source,
        rewrites: Vec::new(),
        references_not_updated,
    }
}

/// Distills the request into its two verified project paths, holding the
/// error arms for a malformed path and a destination equal to the source.
fn move_targets(params: &MoveFileParams) -> Result<(CoreProjectPath, CoreProjectPath), PlanEnd> {
    let path = |field: &'static str, value: &str| {
        CoreProjectPath::new(value).map_err(|error| {
            PlanEnd::from(ReadFault::invalid(
                field,
                rift_core::fault_label(&error.fault().violation()),
            ))
        })
    };
    let from = path("from", &params.from.0)?;
    let to = path("to", &params.to.0)?;
    if from == to {
        return Err(ReadFault::invalid("to", "the destination equals the source").into());
    }
    Ok((from, to))
}

/// Resolves the moved file's current bytes directly from the filesystem, and its language
/// segment when the syntax index claims the path.
///
/// Any visible regular file is movable: absence from the syntax index only means no
/// language identity exists to ask an engine with, which [`engine_proposal`] and the
/// move's own warning path already cover. An indexed path already passed the workspace's
/// `[source]` policy at index construction; a path the index does not hold is checked
/// against that policy directly here, so an excluded path stays unreachable regardless of
/// whether a provider would otherwise claim it. A missing path refuses `target_exists`; a
/// directory refuses `target_is_file`.
async fn resolved_source(
    reads: &ReadService,
    workspace_root: &Path,
    from: &CoreProjectPath,
) -> Result<MovedSource, PlanEnd> {
    let language_segment = reads
        .index()
        .file(from)
        .map(|file| file.syntax().language().identity_segment());
    let absolute = workspace_root.join(from.as_str());
    if language_segment.is_none() && !source_visible(reads, &absolute) {
        return Err(PlanEnd::Refused(crate::publish::not_visible_refusal(from)));
    }
    let metadata = match tokio::fs::metadata(&absolute).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(PlanEnd::Refused(missing_source_refusal(from)));
        }
        Err(error) => {
            return Err(PlanEnd::from(ReadFault::storage(
                from.as_str(),
                "stat",
                &error,
            )));
        }
    };
    if !metadata.is_file() {
        return Err(PlanEnd::Refused(directory_source_refusal(from)));
    }
    let text = tokio::fs::read_to_string(&absolute)
        .await
        .map_err(|error| PlanEnd::from(ReadFault::storage(from.as_str(), "read", &error)))?;
    Ok(MovedSource {
        language_segment,
        text,
    })
}

/// Whether `absolute` is visible under the workspace's `[source]` policy, checked directly
/// for a path the syntax index does not claim - the same policy an indexed path already
/// passed at index construction.
fn source_visible(reads: &ReadService, absolute: &Path) -> bool {
    reads
        .source_policy()
        .is_some_and(|policy| policy.visible(absolute))
}

/// The moved file's source condition failed: it does not exist.
fn missing_source_refusal(path: &CoreProjectPath) -> ChangeResult {
    ChangeResult::refused(
        RefusalReason::UnmetPrecondition,
        vec![failed_precondition(
            OperationPreconditionKind::TargetExists,
            &[],
            path,
            PreconditionValue::Boolean { value: true },
            PreconditionValue::Boolean { value: false },
        )],
    )
}

/// The moved file's source condition failed: a directory occupies the path.
fn directory_source_refusal(path: &CoreProjectPath) -> ChangeResult {
    ChangeResult::refused(
        RefusalReason::UnmetPrecondition,
        vec![failed_precondition(
            OperationPreconditionKind::TargetIsFile,
            &[],
            path,
            PreconditionValue::Boolean { value: true },
            PreconditionValue::Boolean { value: false },
        )],
    )
}

/// Refuses a destination something already occupies on disk, symlinks
/// included.
async fn refused_occupied_destination(
    workspace_root: &Path,
    to: &CoreProjectPath,
) -> Result<(), PlanEnd> {
    let absolute = workspace_root.join(to.as_str());
    match tokio::fs::symlink_metadata(&absolute).await {
        Ok(_) => Err(PlanEnd::Refused(ChangeResult::refused(
            RefusalReason::UnmetPrecondition,
            vec![failed_precondition(
                OperationPreconditionKind::TargetExists,
                &[],
                to,
                PreconditionValue::Boolean { value: false },
                PreconditionValue::Boolean { value: true },
            )],
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ReadFault::storage(to.as_str(), "read", &error).into()),
    }
}

/// Refuses a destination the workspace's `[source]` policy makes
/// invisible. Absence from the syntax index only means no engine can
/// update references at that path; visibility is a separate policy the
/// destination must clear before the file lands there, checked here
/// synchronously alongside the async occupancy check above.
fn refused_invisible_destination(
    reads: &ReadService,
    workspace_root: &Path,
    to: &CoreProjectPath,
) -> Result<(), PlanEnd> {
    match crate::publish::resolve_write_target(
        reads,
        workspace_root,
        to,
        crate::publish::SymlinkResolution::Resolve,
    )? {
        Ok(_) => Ok(()),
        Err(refusal) => Err(PlanEnd::Refused(refusal)),
    }
}

/// What the engine phase produced for one move.
enum EngineProposal {
    /// The engine contributed no reference updates; the reason rides the
    /// applied move as its warning.
    Nothing(ReferencesNotUpdated),
    /// The engine proposed at least one reference edit.
    Answered {
        edit: WorkspaceEdit,
        encoding: PositionEncoding,
    },
}

/// One will-rename exchange's outcome on a running session.
enum MoveExchange {
    /// The engine's filters do not cover the moved file.
    FilterMismatch,
    /// The engine answered, proposing an edit or `null`.
    Answered {
        edit: Option<WorkspaceEdit>,
        encoding: PositionEncoding,
        /// The engine's own readiness as of this answer, read right after
        /// it: whatever the answer says, this is what the engine had
        /// proven about itself when it said it.
        readiness: EngineReadiness,
    },
}

/// Asks the engine serving the moved file's language for reference
/// updates, when one exists and its capability covers the file. `None`
/// names a file no syntax provider claims: no language identity exists to
/// ask about, so no engine is queried at all.
async fn engine_proposal(
    engines: &EnginePool,
    from: &CoreProjectPath,
    to: &CoreProjectPath,
    language_segment: Option<&str>,
    source: &str,
) -> Result<EngineProposal, PlanEnd> {
    let Some(language_segment) = language_segment else {
        return Ok(EngineProposal::Nothing(
            ReferencesNotUpdated::NoLanguageClaimed,
        ));
    };
    let language = crate::rename::segment_language(language_segment);
    let Some(slot) = engines.engine_for(&language) else {
        return Ok(EngineProposal::Nothing(ReferencesNotUpdated::NoEngine {
            language_segment: language_segment.to_owned(),
        }));
    };
    let engine = || slot.name().to_owned();
    match exchanged_will_rename(slot, from, to, &language.name, source).await {
        Ok(MoveExchange::FilterMismatch) => Ok(EngineProposal::Nothing(
            ReferencesNotUpdated::FilterMismatch { engine: engine() },
        )),
        Ok(MoveExchange::Answered {
            edit,
            encoding,
            readiness,
        }) => {
            if proposes_no_edit(edit.as_ref()) {
                Ok(EngineProposal::Nothing(ReferencesNotUpdated::no_edit(
                    engine(),
                    readiness,
                )))
            } else {
                Ok(EngineProposal::Answered {
                    edit: edit.unwrap_or_default(),
                    encoding,
                })
            }
        }
        Err(error) => {
            if matches!(error.fault(), EngineFault::CapabilityAbsent { .. }) {
                return Ok(EngineProposal::Nothing(
                    ReferencesNotUpdated::CapabilityAbsent { engine: engine() },
                ));
            }
            Err(PlanEnd::Failed(ReadFault::engine(error)))
        }
    }
}

/// Runs one will-rename request on the claimed engine's slot.
async fn exchanged_will_rename(
    slot: &EngineSlot,
    from: &CoreProjectPath,
    to: &CoreProjectPath,
    language_id: &str,
    source: &str,
) -> Result<MoveExchange, EngineError> {
    let open_path = from.clone();
    let open_language = language_id.to_owned();
    let open_source = source.to_owned();
    let request_from = from.clone();
    let request_to = to.clone();
    let close_path = from.clone();
    slot.request_exchange(
        move |session: &mut EngineSession| {
            let path = open_path.clone();
            let language = open_language.clone();
            let source = open_source.clone();
            Box::pin(async move { session.open(&path, &language, source).await })
        },
        move |session: &mut EngineSession| {
            let from = request_from.clone();
            let to = request_to.clone();
            Box::pin(async move { will_rename_on_session(session, &from, &to).await })
        },
        move |session: &mut EngineSession| {
            let path = close_path.clone();
            Box::pin(async move {
                let _ = session.close(&path).await;
            })
        },
    )
    .await
}

/// One will-rename request on an open document.
/// Engine whose filters do not cover file is never asked.
async fn will_rename_on_session(
    session: &mut EngineSession,
    from: &CoreProjectPath,
    to: &CoreProjectPath,
) -> Result<MoveExchange, EngineError> {
    let capabilities = session.capabilities();
    let encoding = capabilities.position_encoding;
    if capabilities.will_rename_files() && !capabilities.will_rename_matches(from.as_str()) {
        return Ok(MoveExchange::FilterMismatch);
    }
    let edit = session.will_rename_files(from, to).await?;
    let readiness = session.readiness();
    Ok(MoveExchange::Answered {
        edit,
        encoding,
        readiness,
    })
}

/// Compiles the engine's proposal through the rename kernel and splits the
/// moved file's own rewrite from the reference rewrites.
///
/// An edit addressed to either the source or the destination path applies
/// to the moved file's bytes, and the edited bytes land at the
/// destination.
async fn compiled_move(
    workspace_root: &Path,
    edit: &WorkspaceEdit,
    encoding: PositionEncoding,
    from: CoreProjectPath,
    to: CoreProjectPath,
    moved_source: String,
) -> Result<MovePlan, PlanEnd> {
    let tree_root = workspace_tree_root(workspace_root)?;
    let context = ProposalContext {
        operation: MOVE_OPERATION,
        addresses: Vec::new(),
        opened: None,
        bases: BTreeMap::from([(&from, moved_source.as_str()), (&to, moved_source.as_str())]),
    };
    let documents = proposal_documents(edit, &tree_root, &context)?;
    let documents = merged_moved_documents(documents, &from, &to);
    let compiled = compiled_rewrites(workspace_root, documents, encoding, &context).await?;
    let mut moved_next = moved_source.clone();
    let mut rewrites = Vec::with_capacity(compiled.len());
    for rewrite in compiled {
        if rewrite.path == from {
            moved_next = rewrite.next_source;
        } else {
            rewrites.push(rewrite);
        }
    }
    Ok(MovePlan {
        from,
        to,
        moved_source,
        moved_next,
        rewrites,
        references_not_updated: None,
    })
}

/// Files the destination path's edits under the source path: both address
/// the moved file's bytes, and one merged list keeps the kernel's overlap
/// check over the whole set.
fn merged_moved_documents(
    documents: Vec<(CoreProjectPath, Vec<TextEdit>)>,
    from: &CoreProjectPath,
    to: &CoreProjectPath,
) -> Vec<(CoreProjectPath, Vec<TextEdit>)> {
    let mut merged: BTreeMap<CoreProjectPath, Vec<TextEdit>> = BTreeMap::new();
    for (path, edits) in documents {
        let filed = if &path == to { from.clone() } else { path };
        merged.entry(filed).or_default().extend(edits);
    }
    merged.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rift_core::{SourceVisibility, TextFileInclusion};
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::configuration::HistoryConfiguration;
    use rift_protocol::read::ProjectPath;

    use super::*;

    fn params(from: &str, to: &str) -> MoveFileParams {
        MoveFileParams {
            from: ProjectPath(from.to_owned()),
            to: ProjectPath(to.to_owned()),
        }
    }

    fn workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, ReadService, EnginePool) {
        let directory = tempfile::tempdir().expect("fixture directory");
        for (name, source) in files {
            fs::write(directory.path().join(name), source).expect("fixture file writes");
        }
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )
        .expect("fixture workspace indexes");
        let engines = EnginePool::new(directory.path(), BTreeMap::new());
        (directory, reads, engines)
    }

    fn refused(resolution: MoveResolution) -> ChangeResult {
        match resolution {
            MoveResolution::Refused(result) => result,
            MoveResolution::Planned(plan) => panic!("expected a refusal, got plan {plan:?}"),
        }
    }

    /// One framed JSON-RPC message.
    fn framed(body: &str) -> String {
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    /// A workspace served by a canned `sh` engine that answers `initialize`
    /// advertising will-rename over every path, announces and ends one
    /// work-done progress token before the move ever asks it anything, then
    /// answers the one `workspace/willRenameFiles` request the move sends
    /// with `null` - the engine's own settled verdict that the move needs
    /// no reference rewrite, given only after it has already proven it
    /// reports its own work. The script never reads its stdin; it writes
    /// this fixed sequence regardless of what the session sends.
    fn workspace_with_confirmed_ready_engine(
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, ReadService, EnginePool) {
        let (directory, reads, _unused_engines) = workspace(files);
        let capabilities = framed(
            r#"{"jsonrpc":"2.0","id":0,"result":{"capabilities":{"workspace":{"fileOperations":{"willRename":{"filters":[{"pattern":{"glob":"**/*"}}]}}}}}}"#,
        );
        let progress_begin = framed(
            r#"{"jsonrpc":"2.0","method":"$/progress","params":{"token":"warm","value":{"kind":"begin","title":"loading"}}}"#,
        );
        let progress_end = framed(
            r#"{"jsonrpc":"2.0","method":"$/progress","params":{"token":"warm","value":{"kind":"end"}}}"#,
        );
        let no_edit = framed(r#"{"jsonrpc":"2.0","id":1,"result":null}"#);
        let no_edit_again = framed(r#"{"jsonrpc":"2.0","id":2,"result":null}"#);
        let script = format!(
            "printf '%s' '{capabilities}{progress_begin}{progress_end}{no_edit}{no_edit_again}'; sleep 0.2"
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

    fn precondition(result: &ChangeResult) -> &rift_protocol::change::OperationPrecondition {
        match result {
            ChangeResult::Refused { preconditions, .. } => {
                preconditions.first().expect("a failed condition rides")
            }
            ChangeResult::Applied { .. } => panic!("expected a refusal, got an applied change"),
            ChangeResult::Unchanged => panic!("expected a refusal, got unchanged result"),
        }
    }

    /// Reproduces the defect a `never_announced`-gated warning left behind:
    /// an engine that has already proven it reports its own work, then
    /// answers `null` to `workspace/willRenameFiles`, must still carry
    /// `references_not_updated` - the move's contract permits landing
    /// without a reference rewrite, but never silently.
    #[tokio::test]
    async fn a_move_a_confirmed_ready_engine_answers_nothing_for_still_carries_the_warning() {
        let (directory, reads, engines) =
            workspace_with_confirmed_ready_engine(&[("lib.rs", "pub fn beacon() {}\n")]);
        let resolution = plan_move(
            &reads,
            &engines,
            directory.path(),
            &params("lib.rs", "moved.rs"),
        )
        .await
        .expect("the move plans");
        let MoveResolution::Planned(plan) = resolution else {
            panic!("a move an engine answers null for still plans: {resolution:?}");
        };
        assert!(
            matches!(
                plan.references_not_updated,
                Some(ReferencesNotUpdated::NoReferenceEdits { ref engine }) if engine == "fake"
            ),
            "an engine that confirmed its own readiness and still proposed nothing must carry \
             the warning under its own settled reason, not silence: {:?}",
            plan.references_not_updated
        );
        engines.shutdown().await;
    }

    /// A workspace served by a canned `sh` engine that answers `initialize`
    /// advertising will-rename over every path, never announces any
    /// `$/progress` work of its own, and answers the one
    /// `workspace/willRenameFiles` request the move sends with `null`. With
    /// `retry.attempts` at 1 no empty-answer resend is available, so answer reaches `plan_move`
    /// exactly as the engine gave it: nothing, from an engine that has
    /// proven nothing about its own readiness. The script never reads its
    /// stdin; it writes this fixed sequence regardless of what the session
    /// sends.
    fn workspace_with_an_unconfirmed_engine(
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, ReadService, EnginePool) {
        let (directory, reads, _unused_engines) = workspace(files);
        let capabilities = framed(
            r#"{"jsonrpc":"2.0","id":0,"result":{"capabilities":{"workspace":{"fileOperations":{"willRename":{"filters":[{"pattern":{"glob":"**/*"}}]}}}}}}"#,
        );
        let no_edit = framed(r#"{"jsonrpc":"2.0","id":1,"result":null}"#);
        let script = format!("printf '%s' '{capabilities}{no_edit}'; sleep 0.2");
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

    /// The other half of [`no_edit`](ReferencesNotUpdated::no_edit): an
    /// engine that proposed nothing while its own readiness stayed
    /// unconfirmed carries `AnsweredNothing`, not the settled
    /// `NoReferenceEdits` reason the confirmed-ready engine above earns.
    #[tokio::test]
    async fn a_move_an_unconfirmed_engine_answers_nothing_for_carries_answered_nothing() {
        let (directory, reads, engines) =
            workspace_with_an_unconfirmed_engine(&[("lib.rs", "pub fn beacon() {}\n")]);
        let resolution = plan_move(
            &reads,
            &engines,
            directory.path(),
            &params("lib.rs", "moved.rs"),
        )
        .await
        .expect("the move plans");
        let MoveResolution::Planned(plan) = resolution else {
            panic!("a move an engine answers null for still plans: {resolution:?}");
        };
        assert!(
            matches!(
                plan.references_not_updated,
                Some(ReferencesNotUpdated::AnsweredNothing { ref engine }) if engine == "fake"
            ),
            "an engine that never confirmed its own readiness and proposed nothing must carry \
             the answered-nothing reason, not the settled one: {:?}",
            plan.references_not_updated
        );
        engines.shutdown().await;
    }

    #[tokio::test]
    async fn plan_without_an_engine_names_the_absent_engine() {
        let (directory, reads, engines) = workspace(&[("lib.rs", "pub fn beacon() {}\n")]);
        let resolution = plan_move(
            &reads,
            &engines,
            directory.path(),
            &params("lib.rs", "moved.rs"),
        )
        .await
        .expect("the move plans");
        let MoveResolution::Planned(plan) = resolution else {
            panic!("an engineless move still plans: {resolution:?}");
        };
        assert_eq!(plan.moved_next, plan.moved_source);
        assert!(plan.rewrites.is_empty());
        assert!(matches!(
            plan.references_not_updated,
            Some(ReferencesNotUpdated::NoEngine { ref language_segment })
                if language_segment == "rust"
        ));
    }

    #[tokio::test]
    async fn missing_source_refuses_target_exists() {
        let (directory, reads, engines) = workspace(&[("lib.rs", "pub fn beacon() {}\n")]);
        let result = refused(
            plan_move(
                &reads,
                &engines,
                directory.path(),
                &params("vanished.rs", "moved.rs"),
            )
            .await
            .expect("the refusal is typed"),
        );
        let condition = precondition(&result);
        assert_eq!(condition.kind, OperationPreconditionKind::TargetExists);
        assert_eq!(condition.paths, vec![ProjectPath("vanished.rs".to_owned())]);
        assert_eq!(
            condition.expected,
            PreconditionValue::Boolean { value: true }
        );
    }

    /// Reproduces the defect a reused `target_exists` precondition left behind: a plain
    /// visible file present on disk, that no syntax provider claims, plainly exists and
    /// must move - never refuse as though it were absent.
    #[tokio::test]
    async fn a_source_no_syntax_provider_claims_still_moves_and_names_the_unclaimed_language() {
        let (directory, reads, engines) = workspace(&[("notes.txt", "hello\n")]);
        let resolution = plan_move(
            &reads,
            &engines,
            directory.path(),
            &params("notes.txt", "moved.txt"),
        )
        .await
        .expect("a visible regular file no provider claims is still movable");
        let MoveResolution::Planned(plan) = resolution else {
            panic!("an unindexed visible file must still plan a move: {resolution:?}");
        };
        assert_eq!(plan.moved_next, "hello\n");
        assert!(plan.rewrites.is_empty());
        assert!(
            matches!(
                plan.references_not_updated,
                Some(ReferencesNotUpdated::NoLanguageClaimed)
            ),
            "absence from the syntax index must never refuse the move, and must name the \
             unclaimed language, not a false target_exists: {:?}",
            plan.references_not_updated
        );
        engines.shutdown().await;
    }

    #[tokio::test]
    async fn directory_as_move_source_refuses_target_is_file() {
        let (directory, reads, engines) = workspace(&[("lib.rs", "pub fn beacon() {}\n")]);
        fs::create_dir(directory.path().join("adir")).expect("fixture directory creates");
        let result = refused(
            plan_move(
                &reads,
                &engines,
                directory.path(),
                &params("adir", "moved.rs"),
            )
            .await
            .expect("the refusal is typed"),
        );
        let condition = precondition(&result);
        assert_eq!(condition.kind, OperationPreconditionKind::TargetIsFile);
        assert_eq!(condition.paths, vec![ProjectPath("adir".to_owned())]);
        assert_eq!(
            condition.expected,
            PreconditionValue::Boolean { value: true }
        );
        assert_eq!(
            condition.observed,
            PreconditionValue::Boolean { value: false }
        );
        assert!(
            directory.path().join("adir").is_dir(),
            "the directory is untouched"
        );
        assert!(!directory.path().join("moved.rs").exists());
    }

    #[tokio::test]
    async fn source_excluded_by_policy_refuses_unsupported() {
        let directory = tempfile::tempdir().expect("fixture directory");
        fs::create_dir(directory.path().join("excluded")).expect("fixture directory creates");
        fs::write(directory.path().join("excluded/hidden.txt"), "hidden\n").expect("fixture write");
        let visibility =
            rift_core::SourceVisibility::new(Vec::new(), vec!["excluded/**".to_owned()], true);
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )
        .expect("fixture workspace indexes");
        let engines = EnginePool::new(directory.path(), BTreeMap::new());
        let result = refused(
            plan_move(
                &reads,
                &engines,
                directory.path(),
                &params("excluded/hidden.txt", "moved.txt"),
            )
            .await
            .expect("the refusal is typed"),
        );
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = result
        else {
            panic!("an excluded source must refuse");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.message.contains("excluded/hidden.txt")
                    && diagnostic.message.contains("[source]")
            }),
            "the diagnostic must name the excluded source path and the policy: {diagnostics:?}"
        );
        assert!(directory.path().join("excluded/hidden.txt").exists());
    }

    #[tokio::test]
    async fn occupied_destination_refuses_target_exists_inverted() {
        let (directory, reads, engines) = workspace(&[
            ("lib.rs", "pub fn beacon() {}\n"),
            ("taken.rs", "pub fn taken() {}\n"),
        ]);
        let result = refused(
            plan_move(
                &reads,
                &engines,
                directory.path(),
                &params("lib.rs", "taken.rs"),
            )
            .await
            .expect("the refusal is typed"),
        );
        let condition = precondition(&result);
        assert_eq!(condition.kind, OperationPreconditionKind::TargetExists);
        assert_eq!(condition.paths, vec![ProjectPath("taken.rs".to_owned())]);
        assert_eq!(
            condition.expected,
            PreconditionValue::Boolean { value: false }
        );
    }

    #[tokio::test]
    async fn destination_excluded_by_source_policy_refuses_unsupported() {
        let directory = tempfile::tempdir().expect("fixture directory");
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n").expect("fixture write");
        let visibility =
            rift_core::SourceVisibility::new(Vec::new(), vec!["excluded/**".to_owned()], true);
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )
        .expect("fixture workspace indexes");
        let engines = EnginePool::new(directory.path(), BTreeMap::new());
        let result = refused(
            plan_move(
                &reads,
                &engines,
                directory.path(),
                &params("lib.rs", "excluded/moved.rs"),
            )
            .await
            .expect("the refusal is typed"),
        );
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = result
        else {
            panic!("an excluded destination must refuse");
        };
        assert_eq!(
            reason,
            RefusalReason::Unsupported,
            "a policy-excluded destination refuses as unsupported, not unmet_precondition"
        );
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.message.contains("excluded/moved.rs")
                    && diagnostic.message.contains("[source]")
            }),
            "the diagnostic must name the excluded path and the policy: {diagnostics:?}"
        );
        assert!(
            !directory.path().join("excluded").exists(),
            "a refused move must leave the tree untouched"
        );
        assert!(
            directory.path().join("lib.rs").exists(),
            "the source stays in place"
        );
    }

    /// The source resolves from the filesystem, so a file the index built against one
    /// snapshot but that has since changed on disk moves the bytes actually standing
    /// there rather than a stale copy: there is no earlier snapshot left to drift from.
    #[tokio::test]
    async fn a_source_changed_since_indexing_moves_its_current_disk_bytes() {
        let (directory, reads, engines) = workspace(&[("lib.rs", "pub fn beacon() {}\n")]);
        fs::write(directory.path().join("lib.rs"), "pub fn drifted() {}\n")
            .expect("fixture file writes");
        let resolution = plan_move(
            &reads,
            &engines,
            directory.path(),
            &params("lib.rs", "moved.rs"),
        )
        .await
        .expect("the move plans");
        let MoveResolution::Planned(plan) = resolution else {
            panic!("a changed but still-present source still plans: {resolution:?}");
        };
        assert_eq!(plan.moved_next, "pub fn drifted() {}\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_destination_directory_is_a_storage_failure() {
        use std::os::unix::fs::PermissionsExt;
        let (directory, reads, engines) = workspace(&[("lib.rs", "pub fn beacon() {}\n")]);
        let sealed = directory.path().join("sealed");
        fs::create_dir(&sealed).expect("fixture directory creates");
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000))
            .expect("fixture permissions set");
        let result = plan_move(
            &reads,
            &engines,
            directory.path(),
            &params("lib.rs", "sealed/moved.rs"),
        )
        .await;
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755))
            .expect("fixture permissions restore");
        let error = result.expect_err("an unreadable destination is a storage failure");
        assert_eq!(error.descriptor().code(), "storage_failure");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_source_directory_is_a_storage_failure() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("fixture directory");
        let sealed = directory.path().join("sealed");
        fs::create_dir(&sealed).expect("fixture directory creates");
        fs::write(sealed.join("lib.rs"), "pub fn beacon() {}\n").expect("fixture file writes");
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )
        .expect("fixture workspace indexes");
        let engines = EnginePool::new(directory.path(), BTreeMap::new());
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000))
            .expect("fixture permissions set");
        let result = plan_move(
            &reads,
            &engines,
            directory.path(),
            &params("sealed/lib.rs", "moved.rs"),
        )
        .await;
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755))
            .expect("fixture permissions restore");
        let error = result.expect_err("an unreadable source is a storage failure");
        assert_eq!(error.descriptor().code(), "storage_failure");
        engines.shutdown().await;
    }

    /// A file the index still holds a stale entry for, but that vanished from disk since,
    /// resolves through the same fresh filesystem check as a source that was never
    /// indexed: it refuses `target_exists`, not a raw storage failure.
    #[tokio::test]
    async fn a_source_gone_from_disk_refuses_target_exists() {
        let (directory, reads, engines) = workspace(&[("lib.rs", "pub fn beacon() {}\n")]);
        fs::remove_file(directory.path().join("lib.rs")).expect("fixture file removes");
        let result = refused(
            plan_move(
                &reads,
                &engines,
                directory.path(),
                &params("lib.rs", "moved.rs"),
            )
            .await
            .expect("the refusal is typed"),
        );
        let condition = precondition(&result);
        assert_eq!(condition.kind, OperationPreconditionKind::TargetExists);
        assert_eq!(condition.paths, vec![ProjectPath("lib.rs".to_owned())]);
    }

    #[tokio::test]
    async fn malformed_and_equal_paths_fail_as_invalid_requests() {
        let (directory, reads, engines) = workspace(&[("lib.rs", "pub fn beacon() {}\n")]);
        for (from, to) in [
            ("lib.rs", ".rift/x.rs"),
            ("lib.rs", "../escape.rs"),
            ("/etc/passwd", "moved.rs"),
            ("lib.rs", "lib.rs"),
        ] {
            let error = plan_move(&reads, &engines, directory.path(), &params(from, to))
                .await
                .expect_err("an illegal request fails before resolution");
            assert_eq!(error.descriptor().code(), "invalid_request", "{from}->{to}");
        }
    }

    #[tokio::test]
    async fn oversized_moved_file_refuses_unsupported() {
        let oversized = format!(
            "// {}\n",
            "x".repeat(crate::rewrite::REWRITE_FILE_BYTES_MAX)
        );
        let (directory, reads, engines) = workspace(&[("lib.rs", oversized.as_str())]);
        let result = refused(
            plan_move(
                &reads,
                &engines,
                directory.path(),
                &params("lib.rs", "moved.rs"),
            )
            .await
            .expect("the refusal is typed"),
        );
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = result
        else {
            panic!("an oversized file refuses");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert!(diagnostics[0].message.contains("bytes"));
    }

    #[test]
    fn merged_moved_documents_files_destination_edits_under_the_source() {
        let from = CoreProjectPath::new("old.rs").expect("fixture path");
        let to = CoreProjectPath::new("new.rs").expect("fixture path");
        let other = CoreProjectPath::new("main.rs").expect("fixture path");
        let edit = |text: &str| TextEdit {
            range: lsp_types::Range::default(),
            new_text: text.to_owned(),
        };
        let merged = merged_moved_documents(
            vec![
                (from.clone(), vec![edit("a")]),
                (to.clone(), vec![edit("b")]),
                (other.clone(), vec![edit("c")]),
            ],
            &from,
            &to,
        );
        assert_eq!(merged.len(), 2);
        let moved = merged
            .iter()
            .find(|(path, _)| path == &from)
            .expect("the moved file's edits merge");
        assert_eq!(moved.1.len(), 2, "both URIs' edits file under the source");
    }

    #[test]
    fn every_reason_names_itself_and_carries_the_move_code() {
        let reasons = [
            (
                ReferencesNotUpdated::NoLanguageClaimed,
                "no syntax provider claims this file's language",
            ),
            (
                ReferencesNotUpdated::NoEngine {
                    language_segment: "rust".to_owned(),
                },
                "no engine is configured for language rust",
            ),
            (
                ReferencesNotUpdated::CapabilityAbsent {
                    engine: "fake".to_owned(),
                },
                "does not advertise workspace/willRenameFiles",
            ),
            (
                ReferencesNotUpdated::FilterMismatch {
                    engine: "fake".to_owned(),
                },
                "filters do not cover the moved file",
            ),
            (
                ReferencesNotUpdated::AnsweredNothing {
                    engine: "fake".to_owned(),
                },
                "has announced no work of its own",
            ),
            (
                ReferencesNotUpdated::NoReferenceEdits {
                    engine: "fake".to_owned(),
                },
                "proposed no reference edits for the move",
            ),
        ];
        for (reason, expected) in reasons {
            let diagnostic = reason.diagnostic();
            assert_eq!(diagnostic.severity, Severity::Warning);
            assert_eq!(
                diagnostic.code.as_deref(),
                Some("rift.move.references_not_updated")
            );
            assert!(
                diagnostic.message.contains(expected),
                "{}",
                diagnostic.message
            );
        }
    }
}
