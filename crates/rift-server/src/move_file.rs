//! File move through a configured language engine's will-rename request.
//!
//! The server moves one visible file and asks the language engine for
//! reference updates; the proposal compiles through the rename kernel into
//! whole-file rewrites, and the move and every rewrite land through one
//! atomic publish. Without an engine, without the will-rename capability,
//! or with filters that do not cover the file, the move still lands and
//! the result carries a warning that references were not updated. So does
//! a move an engine answered with no edit at all while it has announced
//! no work of its own: the slot has already asked it twice by then, and a
//! silence from an engine that never says what it is doing is not proof
//! that nothing needed updating.

use std::collections::BTreeMap;
use std::path::Path;

use lsp_types::{TextEdit, WorkspaceEdit};
use rift_core::ProjectPath as CoreProjectPath;
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::session::{EngineError, EngineFault, EngineSession, proposes_no_edit};
use rift_protocol::change::{
    ChangeResult, MoveFileParams, OperationPreconditionKind, PreconditionValue, RefusalReason,
};
use rift_protocol::read::{Diagnostic, DiagnosticCode, Severity};

use crate::engine::{EnginePool, EngineSlot};
use crate::read::{ReadError, ReadFault, ReadService, digest_hex8};
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
/// The first three name an engine that was never asked. The fourth names
/// one that was: it answered nothing, and it has never announced any work
/// of its own, so nothing distinguishes that answer from the answer of an
/// engine with nothing to update.
#[derive(Debug)]
pub(crate) enum ReferencesNotUpdated {
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
    /// The engine proposed no edit and has never announced any work.
    AnsweredNothing {
        /// The engine's configured name.
        engine: String,
    },
}

impl ReferencesNotUpdated {
    /// The warning an applied move carries, naming why its references
    /// were not updated.
    pub(crate) fn diagnostic(&self) -> Diagnostic {
        let reason = match self {
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
    let source = resolved_source(reads, &from)?;
    refused_occupied_destination(workspace_root, &to).await?;
    refused_invisible_destination(reads, workspace_root, &to)?;
    verified_moved_bytes(workspace_root, &from, &source.text).await?;
    refused_oversized(&from, source.text.len(), MOVE_OPERATION)?;
    let proposal = engine_proposal(engines, &from, &to, &source.language_segment).await?;
    match proposal {
        EngineProposal::Nothing(reason) => Ok(unedited_plan(from, to, source.text, Some(reason))),
        EngineProposal::Answered { edit: None, .. } => {
            Ok(unedited_plan(from, to, source.text, None))
        }
        EngineProposal::Answered {
            edit: Some(edit),
            encoding,
        } => compiled_move(workspace_root, &edit, encoding, from, to, source.text).await,
    }
}

/// The moved file's language segment and verified snapshot bytes.
struct MovedSource {
    language_segment: String,
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

/// The moved file's indexed source, or the refusal for a path the served
/// snapshot does not hold - missing, invisible, or not a file.
fn resolved_source(reads: &ReadService, from: &CoreProjectPath) -> Result<MovedSource, PlanEnd> {
    let Some(file) = reads.index().file(from) else {
        return Err(PlanEnd::Refused(ChangeResult::refused(
            RefusalReason::UnmetPrecondition,
            vec![failed_precondition(
                OperationPreconditionKind::TargetExists,
                &[],
                from,
                PreconditionValue::Boolean { value: true },
                PreconditionValue::Boolean { value: false },
            )],
        )));
    };
    Ok(MovedSource {
        language_segment: file.syntax().language().identity_segment(),
        text: file.source().to_owned(),
    })
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

/// Proves the moved file's disk bytes still match the served snapshot, so
/// the engine and the compile see exactly the bytes that will move.
async fn verified_moved_bytes(
    workspace_root: &Path,
    from: &CoreProjectPath,
    source: &str,
) -> Result<(), PlanEnd> {
    let disk = tokio::fs::read_to_string(workspace_root.join(from.as_str()))
        .await
        .map_err(|error| PlanEnd::from(ReadFault::storage(from.as_str(), "read", &error)))?;
    if disk == source {
        return Ok(());
    }
    Err(PlanEnd::Refused(ChangeResult::refused(
        RefusalReason::UnmetPrecondition,
        vec![failed_precondition(
            OperationPreconditionKind::SourceUnchanged,
            &[],
            from,
            PreconditionValue::Text {
                value: digest_hex8(source),
            },
            PreconditionValue::Text {
                value: digest_hex8(&disk),
            },
        )],
    )))
}

/// What the engine phase produced for one move.
enum EngineProposal {
    /// The engine contributed no reference updates; the reason rides the
    /// applied move as its warning.
    Nothing(ReferencesNotUpdated),
    /// The engine answered the will-rename request.
    Answered {
        edit: Option<WorkspaceEdit>,
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
        /// Whether the engine has yet to announce any work of its own.
        /// The slot has already sent the request again once by the time
        /// this is read, so a `true` here means the engine answered
        /// nothing twice without ever saying what it was doing.
        never_announced: bool,
    },
}

/// Asks the engine serving the moved file's language for reference
/// updates, when one exists and its capability covers the file.
async fn engine_proposal(
    engines: &EnginePool,
    from: &CoreProjectPath,
    to: &CoreProjectPath,
    language_segment: &str,
) -> Result<EngineProposal, PlanEnd> {
    let language = crate::rename::segment_language(language_segment);
    let Some(slot) = engines.engine_for(&language) else {
        return Ok(EngineProposal::Nothing(ReferencesNotUpdated::NoEngine {
            language_segment: language_segment.to_owned(),
        }));
    };
    let engine = || slot.name().to_owned();
    match exchanged_will_rename(slot, from, to).await {
        Ok(MoveExchange::FilterMismatch) => Ok(EngineProposal::Nothing(
            ReferencesNotUpdated::FilterMismatch { engine: engine() },
        )),
        Ok(MoveExchange::Answered {
            ref edit,
            never_announced,
            ..
        }) if never_announced && proposes_no_edit(edit.as_ref()) => Ok(EngineProposal::Nothing(
            ReferencesNotUpdated::AnsweredNothing { engine: engine() },
        )),
        Ok(MoveExchange::Answered { edit, encoding, .. }) => {
            Ok(EngineProposal::Answered { edit, encoding })
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
) -> Result<MoveExchange, EngineError> {
    // The boxed future may only borrow the session, so each attempt gets
    // its own owned copy of the request paths.
    let request_from = from.clone();
    let request_to = to.clone();
    slot.request(move |session: &mut EngineSession| {
        let from = request_from.clone();
        let to = request_to.clone();
        Box::pin(async move { will_rename_on_session(session, &from, &to).await })
    })
    .await
}

/// One will-rename conversation. `didOpen` is not required: the request
/// names the old and new URIs alone. An engine whose filters do not cover
/// the file is never asked; an engine without the capability refuses
/// inside the session's own gate.
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
    Ok(MoveExchange::Answered {
        edit,
        encoding,
        never_announced: session.has_never_announced_work(),
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

    fn precondition(result: &ChangeResult) -> &rift_protocol::change::OperationPrecondition {
        match result {
            ChangeResult::Refused { preconditions, .. } => {
                preconditions.first().expect("a failed condition rides")
            }
            ChangeResult::Applied { .. } => panic!("expected a refusal, got an applied change"),
        }
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

    #[tokio::test]
    async fn drifted_source_refuses_source_unchanged() {
        let (directory, reads, engines) = workspace(&[("lib.rs", "pub fn beacon() {}\n")]);
        fs::write(directory.path().join("lib.rs"), "pub fn drifted() {}\n")
            .expect("fixture file writes");
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
        assert_eq!(condition.kind, OperationPreconditionKind::SourceUnchanged);
        assert_ne!(condition.expected, condition.observed);
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

    #[tokio::test]
    async fn a_source_gone_from_disk_is_a_storage_failure() {
        let (directory, reads, engines) = workspace(&[("lib.rs", "pub fn beacon() {}\n")]);
        fs::remove_file(directory.path().join("lib.rs")).expect("fixture file removes");
        let error = plan_move(
            &reads,
            &engines,
            directory.path(),
            &params("lib.rs", "moved.rs"),
        )
        .await
        .expect_err("an unreadable source is a storage failure");
        assert_eq!(error.descriptor().code(), "storage_failure");
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
