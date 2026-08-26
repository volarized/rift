//! Change resolution and atomic application for the workspace tree.
//!
//! Every change tool resolves its address against the served index, proves
//! its preconditions against the bytes on disk, and only then writes. A
//! resolution that produces no edits is a refusal, and the tree stays
//! untouched. Application is serialized per service, so two concurrent
//! changes collide as one clean refusal rather than as interleaved bytes.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use percent_encoding::percent_decode_str;
use rift_core::ProjectPath as CoreProjectPath;
use rift_protocol::change::{
    ChangeId, ChangeResult, ChangeSummary, Edit, InsertPosition, InsertSymbolParams,
    OperationPrecondition, OperationPreconditionKind, OperationPreconditionStatus, PatchParams,
    PreconditionAddress, PreconditionValue, RefusalReason, ReplaceNodeParams, ReplaceSymbolParams,
};
use rift_protocol::read::{
    Diagnostic, DiagnosticContinuation, DiagnosticReliability, Extensions, FileId, Language,
    Severity, SourceSpan, TextRange,
};
use rift_syntax::{ByteRange, SyntaxDocument, SyntaxSource, registry};
use sha2::{Digest as _, Sha256};

use crate::move_file::MovePlan;
use crate::patch;
use crate::read::{ReadError, ReadFault, ReadService, digest_hex8, file_id, node_witness};
use crate::remove::RemovePlan;
use crate::rename::{PlannedRewrite, RenamePlan, survivor_findings};
use crate::rewrite::{FileRewrite, ReplacedRegion, RewriteKind};

/// Most findings one applied change reports: reparse findings and, for a
/// rewrite that published through a symlink, the warning naming it.
const CHANGE_DIAGNOSTICS_MAX: usize = 16;

/// Most edits one applied change reports, mirroring the bound the
/// `ChangeSummary.edits` field advertises. A batch whose replaced regions
/// outnumber it reports one whole-file edit per rewrite instead of one
/// edit per region, which every writing lane's own file bound keeps under
/// the same ceiling.
const CHANGE_EDITS_MAX: usize = 256;

/// Serialized change application against one workspace tree.
#[derive(Debug)]
pub struct ChangeService {
    root: PathBuf,
    application: Mutex<()>,
}

/// One resolved operation: the file it rewrites and the bytes that replace
/// the addressed range.
#[derive(Debug)]
struct ChangePlan {
    path: CoreProjectPath,
    range: ByteRange,
    text: String,
}

/// What resolution decided before anything was written.
#[derive(Debug)]
enum Resolution {
    Planned(ChangePlan),
    Refused {
        reason: RefusalReason,
        preconditions: Vec<OperationPrecondition>,
    },
}

/// Where one `insert_symbol` request lands, once `anchor`/`file`/
/// `create_missing` have been proven mutually consistent.
#[derive(Debug)]
enum InsertTarget<'a> {
    /// Insert beside the resolved declaration of the named anchor symbol.
    BesideAnchor(&'a str),
    /// Insert at the boundary of the named file, creating it first when
    /// `create_missing` is set and it does not exist.
    AtFile {
        /// File the body lands in.
        file: &'a rift_protocol::read::ProjectPath,
        /// Whether a missing file may be created instead of refusing.
        create_missing: bool,
    },
}

/// Classifies an `insert_symbol` request into its target, holding every
/// refusal arm for an illegal combination of `anchor`, `file`, and
/// `create_missing`.
///
/// # Errors
///
/// Returns [`ReadError`] when both or neither of `anchor` and `file` are
/// set, or when `create_missing` is set together with an `anchor` target.
fn insert_target(params: &InsertSymbolParams) -> Result<InsertTarget<'_>, ReadError> {
    match (params.anchor.as_ref(), params.file.as_ref()) {
        (Some(_), Some(_)) => Err(ReadFault::invalid(
            "file",
            "insert_symbol accepts exactly one of anchor or file, not both",
        )),
        (None, None) => Err(ReadFault::invalid(
            "anchor",
            "insert_symbol requires exactly one of anchor or file",
        )),
        (Some(_), None) if params.create_missing => Err(ReadFault::invalid(
            "create_missing",
            "cannot be set with an anchor target; an anchor always addresses \
             an existing file",
        )),
        (Some(anchor), None) => Ok(InsertTarget::BesideAnchor(&anchor.0)),
        (None, Some(file)) => Ok(InsertTarget::AtFile {
            file,
            create_missing: params.create_missing,
        }),
    }
}

impl ChangeService {
    /// Builds a change service writing the given workspace root.
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            application: Mutex::new(()),
        }
    }

    /// Replaces one declaration addressed by symbol.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for a malformed address or a filesystem failure;
    /// a resolvable request that cannot land returns a refused
    /// [`ChangeResult`] instead.
    pub fn replace_symbol(
        &self,
        reads: &ReadService,
        params: &ReplaceSymbolParams,
    ) -> Result<ChangeResult, ReadError> {
        if params.region.is_some() {
            return Err(ReadFault::unsupported("region-scoped replacement"));
        }
        let _application = self
            .application
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let address = parse_symbol_address(&params.symbol.0)?;
        let resolution = self.resolve_symbol_spans(reads, &address, |range| ChangePlan {
            path: address.path.clone(),
            range,
            text: params.body.clone(),
        })?;
        self.conclude(reads, resolution)
    }

    /// Inserts a new declaration beside the anchor symbol, or content at a file
    /// target.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for a malformed address, a request naming both or
    /// neither of `anchor` and `file`, `create_missing` set with `anchor`, or a
    /// filesystem failure; a resolvable request that cannot land returns a
    /// refused [`ChangeResult`] instead.
    pub fn insert_symbol(
        &self,
        reads: &ReadService,
        params: &InsertSymbolParams,
    ) -> Result<ChangeResult, ReadError> {
        let _application = self
            .application
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match insert_target(params)? {
            InsertTarget::BesideAnchor(anchor) => {
                self.insert_beside_anchor(reads, anchor, params.position, &params.body)
            }
            InsertTarget::AtFile {
                file,
                create_missing,
            } => self.insert_at_file(reads, file, params.position, &params.body, create_missing),
        }
    }

    /// Inserts a new declaration beside the anchor symbol.
    fn insert_beside_anchor(
        &self,
        reads: &ReadService,
        anchor: &str,
        position: InsertPosition,
        body: &str,
    ) -> Result<ChangeResult, ReadError> {
        let address = parse_symbol_address(anchor)?;
        let body = body.to_owned();
        let resolution = self.resolve_symbol_spans(reads, &address, |range| {
            let (at, text) = match position {
                InsertPosition::Before => (range.start, format!("{body}\n\n")),
                InsertPosition::After => (range.end, format!("\n\n{body}")),
            };
            ChangePlan {
                path: address.path.clone(),
                range: ByteRange { start: at, end: at },
                text,
            }
        })?;
        self.conclude(reads, resolution)
    }

    /// Inserts content at a file target: the start of the file for `before`, the
    /// end for `after`, one blank line from what was already there. A missing
    /// file is created, parent directories included, only when `create_missing`
    /// is set; otherwise resolution refuses, naming the missing target. The body
    /// lands verbatim - no parser involvement - so any project-relative path is
    /// a legal target.
    fn insert_at_file(
        &self,
        reads: &ReadService,
        file: &rift_protocol::read::ProjectPath,
        position: InsertPosition,
        body: &str,
        create_missing: bool,
    ) -> Result<ChangeResult, ReadError> {
        let path = CoreProjectPath::new(file.0.as_str()).map_err(|error| {
            ReadFault::invalid("file", rift_core::fault_label(&error.fault().violation()))
        })?;
        if let Err(refusal) = crate::publish::resolve_write_target(
            reads,
            &self.root,
            &path,
            crate::publish::SymlinkResolution::Resolve,
        )? {
            return Ok(refusal);
        }
        let absolute = self.root.join(path.as_str());
        let existing = match fs::read_to_string(&absolute) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(ReadFault::storage(path.as_str(), "read", &error)),
        };
        let rewrite = match existing {
            Some(content) => {
                let (next_source, region) = spliced_at_file_edge(&content, position, body);
                FileRewrite::modify(path, &content, next_source, vec![region])
            }
            None if create_missing => FileRewrite::create(path, body.to_owned()),
            None => {
                return Ok(ChangeResult::refused(
                    RefusalReason::UnmetPrecondition,
                    vec![OperationPrecondition::new(
                        OperationPreconditionKind::TargetExists,
                        OperationPreconditionStatus::Failed,
                        Vec::new(),
                        vec![path.as_str().to_owned()],
                        PreconditionValue::Boolean { value: true },
                        PreconditionValue::Boolean { value: false },
                    )],
                ));
            }
        };
        self.apply_rewrites(reads, &[rewrite])
    }

    /// Replaces one syntax node through a witnessed address.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for a malformed address or a filesystem failure;
    /// a stale witness returns a refused [`ChangeResult`] instead.
    pub fn replace_node(
        &self,
        reads: &ReadService,
        params: &ReplaceNodeParams,
    ) -> Result<ChangeResult, ReadError> {
        if params.region.is_some() {
            return Err(ReadFault::unsupported("region-scoped replacement"));
        }
        let _application = self
            .application
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match resolve_node(reads, &params.node)? {
            NodeResolution::Refused {
                reason,
                preconditions,
            } => Ok(ChangeResult::refused(reason, preconditions)),
            NodeResolution::Verified { address, .. } => {
                let resolution = self.verified_against_disk(
                    reads,
                    &address.path,
                    ChangePlan {
                        path: address.path.clone(),
                        range: address.range,
                        text: params.body.clone(),
                    },
                )?;
                self.conclude(reads, resolution)
            }
        }
    }

    /// Verifies and writes one engine-proposed rename plan atomically, then
    /// sweeps the changed tree for surviving occurrences of the old name.
    ///
    /// Every rewritten file must still hold the exact bytes the plan was
    /// compiled against: the last precondition before an engine proposal
    /// may write, the same proof `verified_against_disk` runs for the
    /// parser-derived plans.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for a filesystem failure; a file that drifted
    /// since the plan was compiled returns a refused [`ChangeResult`]
    /// instead.
    pub fn apply_rename(
        &self,
        reads: &ReadService,
        plan: &RenamePlan,
    ) -> Result<ChangeResult, ReadError> {
        let _application = self
            .application
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let checks = plan
            .rewrites
            .iter()
            .map(|rewrite| (&rewrite.path, rewrite.base_source.as_str()));
        if let Some(refusal) =
            self.rewrite_precondition_failure(reads, &plan.symbol_addresses(), checks)?
        {
            return Ok(refusal);
        }
        let rewrites: Vec<FileRewrite> = plan.rewrites.iter().map(modify_rewrite).collect();
        let mut result = self.apply_rewrites(reads, &rewrites)?;
        if let ChangeResult::Applied { summary } = &mut result {
            summary.diagnostics.extend(survivor_findings(reads, plan));
        }
        Ok(result)
    }

    /// Verifies and writes one move plan atomically: every reference
    /// rewrite, the destination's new file, and the source's removal land
    /// or roll back together.
    ///
    /// The plan's bases are re-proven against the disk first - the same
    /// proof `apply_rename` runs - plus the move's own conditions: the
    /// source still served, its bytes unchanged, the destination still
    /// absent. An applied move whose engine was skipped carries the
    /// references-not-updated warning on its summary.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for a filesystem failure; a condition that no
    /// longer holds returns a refused [`ChangeResult`] instead.
    pub fn apply_move(
        &self,
        reads: &ReadService,
        plan: &MovePlan,
    ) -> Result<ChangeResult, ReadError> {
        let _application = self
            .application
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let checks = std::iter::once((&plan.from, plan.moved_source.as_str())).chain(
            plan.rewrites
                .iter()
                .map(|rewrite| (&rewrite.path, rewrite.base_source.as_str())),
        );
        if let Some(refusal) = self.rewrite_precondition_failure(reads, &[], checks)? {
            return Ok(refusal);
        }
        if self.root.join(plan.to.as_str()).symlink_metadata().is_ok() {
            return Ok(ChangeResult::refused(
                RefusalReason::UnmetPrecondition,
                vec![OperationPrecondition::new(
                    OperationPreconditionKind::TargetExists,
                    OperationPreconditionStatus::Failed,
                    Vec::new(),
                    vec![plan.to.as_str().to_owned()],
                    PreconditionValue::Boolean { value: false },
                    PreconditionValue::Boolean { value: true },
                )],
            ));
        }
        let mut rewrites: Vec<FileRewrite> = plan.rewrites.iter().map(modify_rewrite).collect();
        rewrites.push(FileRewrite::create(
            plan.to.clone(),
            plan.moved_next.clone(),
        ));
        rewrites.push(FileRewrite::delete(plan.from.clone(), &plan.moved_source));
        rewrites.sort_by(|first, second| first.path.as_str().cmp(second.path.as_str()));
        let mut result = self.apply_rewrites(reads, &rewrites)?;
        if let ChangeResult::Applied { summary } = &mut result
            && let Some(reason) = &plan.references_not_updated
        {
            summary.diagnostics.push(reason.diagnostic());
        }
        Ok(result)
    }

    /// Verifies and writes one planned removal atomically.
    ///
    /// The plan's base is re-proven against the disk first, the same proof `apply_rename`
    /// and `apply_move` run, because the reference check that produced the plan ran before
    /// this call and the tree may have moved since. An applied removal whose reference check
    /// found something standing, or could not run at all, carries that as the summary's
    /// warning.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for a filesystem failure; a file that drifted since the plan
    /// was compiled returns a refused [`ChangeResult`] instead.
    pub fn apply_remove(
        &self,
        reads: &ReadService,
        plan: &RemovePlan,
    ) -> Result<ChangeResult, ReadError> {
        let _application = self
            .application
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let checks = std::iter::once((&plan.path, plan.base_source.as_str()));
        if let Some(refusal) = self.rewrite_precondition_failure(reads, &plan.addresses, checks)? {
            return Ok(refusal);
        }
        let rewrite = FileRewrite::modify(
            plan.path.clone(),
            &plan.base_source,
            plan.next_source.clone(),
            vec![plan.replaced.clone()],
        );
        let mut result = self.apply_rewrites(reads, std::slice::from_ref(&rewrite))?;
        if let ChangeResult::Applied { summary } = &mut result
            && let Some(diagnostic) = &plan.diagnostic
        {
            summary.diagnostics.push(diagnostic.clone());
        }
        Ok(result)
    }

    /// The first rewrite precondition that fails: a planned file gone from
    /// the index, or one whose disk bytes drifted from the bytes the plan
    /// was compiled against. Nothing when every file still matches.
    fn rewrite_precondition_failure<'plan>(
        &self,
        reads: &ReadService,
        addresses: &[PreconditionAddress],
        checks: impl Iterator<Item = (&'plan CoreProjectPath, &'plan str)>,
    ) -> Result<Option<ChangeResult>, ReadError> {
        for (path, base_source) in checks {
            if reads.index().file(path).is_none() {
                return Ok(Some(ChangeResult::refused(
                    RefusalReason::UnmetPrecondition,
                    vec![OperationPrecondition::new(
                        OperationPreconditionKind::TargetExists,
                        OperationPreconditionStatus::Failed,
                        addresses.to_vec(),
                        vec![path.as_str().to_owned()],
                        PreconditionValue::Boolean { value: true },
                        PreconditionValue::Boolean { value: false },
                    )],
                )));
            }
            let disk = fs::read_to_string(self.root.join(path.as_str()))
                .map_err(|error| ReadFault::storage(path.as_str(), "read", &error))?;
            if disk != base_source {
                return Ok(Some(ChangeResult::refused(
                    RefusalReason::UnmetPrecondition,
                    vec![OperationPrecondition::new(
                        OperationPreconditionKind::SourceUnchanged,
                        OperationPreconditionStatus::Failed,
                        addresses.to_vec(),
                        vec![path.as_str().to_owned()],
                        PreconditionValue::Text {
                            value: digest_hex8(base_source),
                        },
                        PreconditionValue::Text {
                            value: digest_hex8(&disk),
                        },
                    )],
                )));
            }
        }
        Ok(None)
    }

    /// Applies unified-diff hunks to workspace files atomically.
    ///
    /// Hunk context locates each hunk; header line numbers are hints, as
    /// with `git apply`. A `/dev/null` header creates or deletes the file.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for a diff that does not parse, addresses an
    /// illegal path, or fails at the filesystem; hunk-context that cannot
    /// be located, and a rename or copy, which this release does not
    /// serve, return a refused [`ChangeResult`] instead.
    pub fn patch(
        &self,
        reads: &ReadService,
        params: &PatchParams,
    ) -> Result<ChangeResult, ReadError> {
        let _application = self
            .application
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let segments = patch::split_file_segments(&params.patch)?;
        let mut rewrites: Vec<FileRewrite> = Vec::with_capacity(segments.len());
        for (index, segment) in segments.iter().enumerate() {
            match patch::resolve_segment(&self.root, reads, segment, index + 1)? {
                Ok(rewrite) => rewrites.push(rewrite),
                Err(refusal) => return Ok(refusal),
            }
        }
        // A diff may address its files in any order; the summary's paths and
        // edits are documented in canonical file order, and the change id
        // digests the rewrites in the order they are applied.
        rewrites.sort_by(|first, second| first.path.as_str().cmp(second.path.as_str()));
        self.apply_rewrites(reads, &rewrites)
    }

    /// Stages and publishes whole-file rewrites, all or none, through
    /// [`crate::publish::publish_rewrites`], then builds the result: only
    /// the filesystem transaction lives there, this keeps building the
    /// [`ChangeResult`].
    fn apply_rewrites(
        &self,
        reads: &ReadService,
        rewrites: &[FileRewrite],
    ) -> Result<ChangeResult, ReadError> {
        let warnings = match crate::publish::publish_rewrites(reads, &self.root, rewrites)? {
            Ok(warnings) => warnings,
            Err(refusal) => return Ok(refusal),
        };
        let ranged = regions_fit_the_edit_bound(rewrites);
        let mut identity = Sha256::new();
        let mut paths = Vec::with_capacity(rewrites.len());
        let mut edits = Vec::with_capacity(rewrites.len());
        let mut diagnostics = warnings;
        for rewrite in rewrites {
            identity.update(rewrite.path.as_str().as_bytes());
            identity.update([0]);
            identity.update(rewrite.next_source.as_bytes());
            paths.push(rift_protocol::read::ProjectPath(
                rewrite.path.as_str().to_owned(),
            ));
            let unit = match reads.index().file(&rewrite.path) {
                Some(file) => file_id(file.path()),
                None => FileId(format!(
                    "rift://file/{}",
                    rift_core::encode_path(rewrite.path.as_str())
                )),
            };
            edits.extend(rewrite_edits(rewrite, &unit, ranged));
            fold_and_bound_diagnostics(&mut diagnostics, rewrite, unit);
        }
        let digest = identity.finalize();
        Ok(ChangeResult::Applied {
            summary: ChangeSummary {
                id: ChangeId(crate::read::digest_wire_hex(&digest)),
                paths,
                edits,
                diagnostics,
                guarantees: Vec::new(),
            },
        })
    }

    /// Resolves one symbol address to its declaration span and builds the
    /// plan through `plan`, refusing when the target is missing.
    fn resolve_symbol_spans(
        &self,
        reads: &ReadService,
        address: &SymbolAddress,
        plan: impl Fn(ByteRange) -> ChangePlan,
    ) -> Result<Resolution, ReadError> {
        match resolve_symbol(reads, address)? {
            SymbolResolution::Refused {
                reason,
                preconditions,
            } => Ok(Resolution::Refused {
                reason,
                preconditions,
            }),
            SymbolResolution::Declared { symbol, .. } => {
                self.verified_against_disk(reads, &address.path, plan(symbol.range))
            }
        }
    }

    /// Proves the indexed bytes still match the file on disk, the last
    /// precondition before a plan may write.
    fn verified_against_disk(
        &self,
        reads: &ReadService,
        path: &CoreProjectPath,
        plan: ChangePlan,
    ) -> Result<Resolution, ReadError> {
        let Some(file) = reads.index().file(path) else {
            return Err(ReadFault::not_found(path.as_str()));
        };
        let disk = fs::read_to_string(self.root.join(path.as_str()))
            .map_err(|error| ReadFault::storage(path.as_str(), "read", &error))?;
        if disk != file.source() {
            return Ok(Resolution::Refused {
                reason: RefusalReason::UnmetPrecondition,
                preconditions: vec![OperationPrecondition::new(
                    OperationPreconditionKind::SourceUnchanged,
                    OperationPreconditionStatus::Failed,
                    Vec::new(),
                    vec![path.as_str().to_owned()],
                    PreconditionValue::Text {
                        value: digest_hex8(file.source()),
                    },
                    PreconditionValue::Text {
                        value: digest_hex8(&disk),
                    },
                )],
            });
        }
        Ok(Resolution::Planned(plan))
    }

    /// Applies a planned change, or returns the refusal unchanged.
    fn conclude(
        &self,
        reads: &ReadService,
        resolution: Resolution,
    ) -> Result<ChangeResult, ReadError> {
        match resolution {
            Resolution::Refused {
                reason,
                preconditions,
            } => Ok(ChangeResult::refused(reason, preconditions)),
            Resolution::Planned(plan) => self.apply(reads, plan),
        }
    }

    /// Writes one plan atomically and reports what landed.
    ///
    /// The public operation holds the application lock from resolution
    /// through this call, so the disk proof cannot race another Rift
    /// write. Publishing goes through [`Self::apply_rewrites`] - the same
    /// gate and staging transaction every other write path shares - so a
    /// symlinked target publishes beside its resolved file rather than
    /// replacing the link.
    fn apply(&self, reads: &ReadService, plan: ChangePlan) -> Result<ChangeResult, ReadError> {
        let Some(file) = reads.index().file(&plan.path) else {
            return Err(ReadFault::not_found(plan.path.as_str()));
        };
        let source = file.source();
        let start = usize::try_from(plan.range.start)
            .ok()
            .filter(|start| *start <= source.len() && source.is_char_boundary(*start));
        let end = usize::try_from(plan.range.end)
            .ok()
            .filter(|end| *end <= source.len() && source.is_char_boundary(*end));
        let (Some(start), Some(end)) = (start, end) else {
            return Err(ReadFault::invalid("span", "outside the addressed file"));
        };
        if start > end {
            return Err(ReadFault::invalid("span", "start beyond end"));
        }
        let mut next_source = String::with_capacity(source.len() - (end - start) + plan.text.len());
        next_source.push_str(&source[..start]);
        next_source.push_str(&plan.text);
        next_source.push_str(&source[end..]);
        let region = ReplacedRegion {
            range: ByteRange {
                start: plan.range.start,
                end: plan.range.end,
            },
            text: plan.text,
        };
        let rewrite = FileRewrite::modify(plan.path, source, next_source, vec![region]);
        self.apply_rewrites(reads, std::slice::from_ref(&rewrite))
    }
}

/// One planned rewrite as the in-place modification of its file, carrying
/// the regions the engine's own edits named.
fn modify_rewrite(rewrite: &PlannedRewrite) -> FileRewrite {
    FileRewrite::modify(
        rewrite.path.clone(),
        &rewrite.base_source,
        rewrite.next_source.clone(),
        rewrite.replaced.clone(),
    )
}

/// Whether this batch's replaced regions fit the bound a change result
/// carries. Past it every rewrite reports its whole file instead, which
/// the file bound each writing lane enforces keeps within the same
/// ceiling.
fn regions_fit_the_edit_bound(rewrites: &[FileRewrite]) -> bool {
    rewrites
        .iter()
        .map(|rewrite| match &rewrite.kind {
            RewriteKind::Modify { replaced } => replaced.len(),
            RewriteKind::Create | RewriteKind::Delete => 1,
        })
        .sum::<usize>()
        <= CHANGE_EDITS_MAX
}

/// The edits one rewrite contributes to a change result: one per replaced
/// region for a modification, and the whole file for a create or a delete,
/// where the whole file is the change. A batch whose regions did not fit
/// the edit bound passes `ranged` as false and reports whole files
/// throughout.
fn rewrite_edits(rewrite: &FileRewrite, unit: &FileId, ranged: bool) -> Vec<Edit> {
    match &rewrite.kind {
        RewriteKind::Modify { replaced } if ranged => replaced
            .iter()
            .map(|region| {
                replace_edit(
                    unit,
                    region.range.start,
                    region.range.end,
                    region.text.clone(),
                )
            })
            .collect(),
        _ => vec![replace_edit(
            unit,
            0,
            rewrite.previous_len,
            rewrite.next_source.clone(),
        )],
    }
}

/// One replacement of `unit`'s bytes from `start` to `end` by `text`.
fn replace_edit(unit: &FileId, start: u64, end: u64, text: String) -> Edit {
    Edit::Replace {
        span: SourceSpan {
            unit: unit.clone(),
            range: TextRange { start, end },
        },
        text,
    }
}

/// Splices new content at a file boundary, matching the anchor-insert spacing
/// policy: one blank line separates the new content from what was already there.
/// The region names where the splice lands in the existing bytes, so the
/// result reports the insert rather than the whole file.
fn spliced_at_file_edge(
    existing: &str,
    position: InsertPosition,
    body: &str,
) -> (String, ReplacedRegion) {
    let (at, text) = match position {
        InsertPosition::Before => (0, format!("{body}\n\n")),
        InsertPosition::After => (existing.len(), format!("\n\n{body}")),
    };
    let mut next_source = existing.to_owned();
    next_source.insert_str(at, &text);
    let at = at as u64;
    (
        next_source,
        ReplacedRegion {
            range: ByteRange { start: at, end: at },
            text,
        },
    )
}

/// A parsed witnessed node address.
#[derive(Debug)]
pub(crate) struct NodeAddress {
    pub(crate) language_segment: String,
    pub(crate) path: CoreProjectPath,
    pub(crate) range: ByteRange,
    pub(crate) witness: String,
}

/// One witnessed node address resolved against the served snapshot: the file holding it and
/// the parsed address, or the refusal resolution produced for a missing file or a witness
/// that no longer matches the source.
#[derive(Debug)]
pub(crate) enum NodeResolution<'reads> {
    /// The address's witness matches the served snapshot's bytes at its range.
    Verified {
        /// The indexed file holding the node.
        file: &'reads rift_index::IndexedFile,
        /// The parsed, witness-verified address.
        address: NodeAddress,
    },
    /// Resolution produced a refusal; the tree stays untouched.
    Refused {
        /// The condition the caller can act on.
        reason: RefusalReason,
        /// The checked conditions, including the failed entry.
        preconditions: Vec<OperationPrecondition>,
    },
}

/// Resolves one witnessed node address: parses it, resolves its file against the served
/// snapshot, verifies its address language, and proves its witness still matches the
/// source. `replace_node` and the remove tools share this resolution.
///
/// # Errors
///
/// Returns [`ReadError`] for a malformed address or a language segment that does not match
/// the indexed file's language.
pub(crate) fn resolve_node<'reads>(
    reads: &'reads ReadService,
    node: &rift_protocol::read::NodeId,
) -> Result<NodeResolution<'reads>, ReadError> {
    let address: NodeAddress = node.0.parse()?;
    let Some(file) = reads.index().file(&address.path) else {
        return Ok(NodeResolution::Refused {
            reason: RefusalReason::UnmetPrecondition,
            preconditions: vec![OperationPrecondition::new(
                OperationPreconditionKind::TargetExists,
                OperationPreconditionStatus::Failed,
                vec![PreconditionAddress::Node { node: node.clone() }],
                vec![address.path.as_str().to_owned()],
                PreconditionValue::Boolean { value: true },
                PreconditionValue::Boolean { value: false },
            )],
        });
    };
    verified_address_language("node", &address.language_segment, file.syntax())?;
    let observed_witness = node_witness(file.source(), address.range);
    if observed_witness != address.witness {
        return Ok(NodeResolution::Refused {
            reason: RefusalReason::UnmetPrecondition,
            preconditions: vec![OperationPrecondition::new(
                OperationPreconditionKind::SourceUnchanged,
                OperationPreconditionStatus::Failed,
                vec![PreconditionAddress::Node { node: node.clone() }],
                vec![address.path.as_str().to_owned()],
                PreconditionValue::Text {
                    value: address.witness,
                },
                PreconditionValue::Text {
                    value: observed_witness,
                },
            )],
        });
    }
    Ok(NodeResolution::Verified { file, address })
}

/// A parsed symbol address: the language segment it files under, and its
/// decoded path and qualified name.
#[derive(Debug)]
pub(crate) struct SymbolAddress {
    pub(crate) language_segment: String,
    pub(crate) path: CoreProjectPath,
    pub(crate) qualified_name: String,
}

impl SymbolAddress {
    /// The wire symbol identity this address spells, re-encoded.
    pub(crate) fn wire_symbol(&self) -> rift_protocol::read::SymbolId {
        rift_protocol::read::SymbolId(rift_core::symbol_identity(
            &self.language_segment,
            self.path.as_str(),
            &self.qualified_name,
        ))
    }
}

/// One symbol address resolved against the served snapshot: the single
/// declaration it names, or the refusal resolution produced.
#[derive(Debug)]
pub(crate) enum SymbolResolution<'reads> {
    /// The address names exactly one declaration in one indexed file.
    Declared {
        /// The indexed file holding the declaration.
        file: &'reads rift_index::IndexedFile,
        /// The declaration the address names.
        symbol: &'reads rift_syntax::SyntaxSymbol,
    },
    /// Resolution produced no single declaration; the tree stays untouched.
    Refused {
        /// The condition the caller can act on.
        reason: RefusalReason,
        /// The checked conditions, including the failed entry.
        preconditions: Vec<OperationPrecondition>,
    },
}

/// Resolves one symbol address to its declaration, refusing when the file is
/// missing or the name resolves to nothing.
///
/// One document holds at most one declaration under a qualified name: the
/// syntax layer suffixes every repeated name apart, so an address naming
/// several declarations cannot be written.
///
/// # Errors
///
/// Returns [`ReadError`] when the address's language segment does not match
/// the indexed file's language.
pub(crate) fn resolve_symbol<'reads>(
    reads: &'reads ReadService,
    address: &SymbolAddress,
) -> Result<SymbolResolution<'reads>, ReadError> {
    let path = &address.path;
    let symbol_addresses = || {
        vec![PreconditionAddress::Symbol {
            symbol: address.wire_symbol(),
        }]
    };
    let missing_target = || SymbolResolution::Refused {
        reason: RefusalReason::UnmetPrecondition,
        preconditions: vec![OperationPrecondition::new(
            OperationPreconditionKind::TargetExists,
            OperationPreconditionStatus::Failed,
            symbol_addresses(),
            vec![path.as_str().to_owned()],
            PreconditionValue::Boolean { value: true },
            PreconditionValue::Boolean { value: false },
        )],
    };
    let Some(file) = reads.index().file(path) else {
        return Ok(missing_target());
    };
    verified_address_language("symbol", &address.language_segment, file.syntax())?;
    let declaration = file
        .syntax()
        .symbols()
        .iter()
        .find(|symbol| symbol.qualified_name == address.qualified_name);
    match declaration {
        Some(symbol) => Ok(SymbolResolution::Declared { file, symbol }),
        None => Ok(missing_target()),
    }
}

/// Splits `rift://symbol/<language>/<path>/<qualified-name>` into its
/// decoded parts. The language segment is taken as spelled; resolution
/// verifies it against the addressed file's document.
pub(crate) fn parse_symbol_address(address: &str) -> Result<SymbolAddress, ReadError> {
    let malformed = || ReadFault::invalid("symbol", "not a rift symbol address");
    let remainder = address
        .strip_prefix("rift://symbol/")
        .ok_or_else(malformed)?;
    let (language_segment, remainder) = remainder.split_once('/').ok_or_else(malformed)?;
    if language_segment.is_empty() {
        return Err(malformed());
    }
    let (encoded_path, encoded_name) = remainder.rsplit_once('/').ok_or_else(malformed)?;
    let path = decoded(encoded_path).ok_or_else(malformed)?;
    let qualified_name = decoded(encoded_name).ok_or_else(malformed)?;
    let path = CoreProjectPath::new(path).map_err(|error| {
        ReadFault::invalid("symbol", rift_core::fault_label(&error.fault().violation()))
    })?;
    Ok(SymbolAddress {
        language_segment: language_segment.to_owned(),
        path,
        qualified_name,
    })
}

impl std::str::FromStr for NodeAddress {
    type Err = ReadError;

    /// Parses `rift://node/<language>/<path>@<start>-<end>#<witness>`.
    fn from_str(address: &str) -> Result<Self, Self::Err> {
        let malformed = || ReadFault::invalid("node", "not a witnessed rift node address");
        let remainder = address.strip_prefix("rift://node/").ok_or_else(malformed)?;
        let (language_segment, remainder) = remainder.split_once('/').ok_or_else(malformed)?;
        if language_segment.is_empty() {
            return Err(malformed());
        }
        let (located, witness) = remainder.rsplit_once('#').ok_or_else(malformed)?;
        let (encoded_path, span) = located.rsplit_once('@').ok_or_else(malformed)?;
        let (start, end) = span.split_once('-').ok_or_else(malformed)?;
        let start: u64 = start.parse().map_err(|_| malformed())?;
        let end: u64 = end.parse().map_err(|_| malformed())?;
        if start > end || witness.len() != 8 {
            return Err(malformed());
        }
        let path = decoded(encoded_path).ok_or_else(malformed)?;
        let path = CoreProjectPath::new(path).map_err(|error| {
            ReadFault::invalid("node", rift_core::fault_label(&error.fault().violation()))
        })?;
        Ok(Self {
            language_segment: language_segment.to_owned(),
            path,
            range: ByteRange { start, end },
            witness: witness.to_owned(),
        })
    }
}

/// Verifies an address's language segment against the addressed file's
/// document, once the file has resolved.
///
/// # Errors
///
/// Returns [`ReadError`] naming both spellings when they differ: an address
/// filed under one language cannot act on a document filed under another.
pub(crate) fn verified_address_language(
    field: &'static str,
    language_segment: &str,
    document: &SyntaxDocument,
) -> Result<(), ReadError> {
    let document_segment = document.language().identity_segment();
    if language_segment == document_segment {
        return Ok(());
    }
    Err(ReadFault::invalid(
        field,
        format!(
            "address language {language_segment} does not match the indexed \
             language {document_segment}"
        ),
    ))
}

fn decoded(encoded: &str) -> Option<String> {
    percent_decode_str(encoded)
        .decode_utf8()
        .ok()
        .map(std::borrow::Cow::into_owned)
}

/// Re-parses the changed file and reports parser findings, bounded.
///
/// A change that breaks the syntax still lands - the tree is the caller's -
/// but the result says so instead of leaving the discovery to the next read.
/// `unit` names the changed file even when it has no prior index entry, as
/// for a file a patch just created. The registry selects the parsing
/// provider by the path's extension; a path no provider claims has no
/// grammar to check against and contributes no findings.
fn reparse_diagnostics(unit: FileId, path: &CoreProjectPath, source: &str) -> Vec<Diagnostic> {
    let Some(provider) = path_extension(path).and_then(registry::provider_for_extension) else {
        return Vec::new();
    };
    let language = provider.language();
    match provider.analyze(SyntaxSource { path, text: source }) {
        Err(error) => vec![change_diagnostic(
            unit,
            format!("the changed file no longer parses within bounds: {error}"),
            None,
            language.clone(),
        )],
        Ok(document) => document
            .nodes()
            .iter()
            .filter(|node| node.has_error)
            .take(CHANGE_DIAGNOSTICS_MAX)
            .map(|node| {
                change_diagnostic(
                    unit.clone(),
                    "the parser marked this region erroneous after the change".to_owned(),
                    Some(node.range),
                    language.clone(),
                )
            })
            .collect(),
    }
}

/// Folds `rewrite`'s reparse diagnostics into the batch's running list,
/// then enforces [`CHANGE_DIAGNOSTICS_MAX`]. A deletion contributes no
/// reparse diagnostics of its own - it is never reparsed - but the bound
/// still applies to it, so warnings carried in from earlier in the batch
/// cannot outlive a batch made only of deletions.
fn fold_and_bound_diagnostics(
    diagnostics: &mut Vec<Diagnostic>,
    rewrite: &FileRewrite,
    unit: FileId,
) {
    if !rewrite.kind.removes_file() {
        diagnostics.extend(reparse_diagnostics(
            unit,
            &rewrite.path,
            &rewrite.next_source,
        ));
    }
    diagnostics.truncate(CHANGE_DIAGNOSTICS_MAX);
}

/// The extension of `path`'s final segment, without its leading dot.
fn path_extension(path: &CoreProjectPath) -> Option<&str> {
    Path::new(path.as_str())
        .extension()
        .and_then(std::ffi::OsStr::to_str)
}

fn change_diagnostic(
    unit: rift_protocol::read::FileId,
    message: String,
    range: Option<ByteRange>,
    language: Language,
) -> Diagnostic {
    Diagnostic {
        severity: Severity::Error,
        code: None,
        message,
        span: range.map(|range| SourceSpan {
            unit,
            range: TextRange {
                start: range.start,
                end: range.end,
            },
        }),
        related: Vec::new(),
        tags: Vec::new(),
        reliability: DiagnosticReliability::Recovered,
        continuation: DiagnosticContinuation::Unknown,
        extensions: Extensions(std::collections::BTreeMap::new()),
        language: Some(language),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::sync::{Arc, Barrier};

    use rift_core::ProjectPath as CoreProjectPath;
    use rift_core::SourceVisibility;
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::change::{
        ChangeResult, Edit, InsertPosition, InsertSymbolParams, OperationPreconditionKind,
        PatchParams, PreconditionAddress, PreconditionValue, RefusalReason, ReplaceNodeParams,
        ReplaceSymbolParams,
    };
    use rift_protocol::configuration::HistoryConfiguration;
    use rift_protocol::read::{
        Diagnostic, DiagnosticContinuation, DiagnosticReliability, Extensions, FileId,
        GetSymbolParams, Language, NodeId, NodesParams, ProjectPath, Severity, SymbolId,
    };
    use rift_syntax::ByteRange;

    use super::ChangeService;
    use crate::read::{ReadService, node_witness};
    use crate::rewrite::{FileRewrite, ReplacedRegion};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn fixture(source: &str) -> TestResult<(tempfile::TempDir, ReadService, ChangeService)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), source)?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        Ok((directory, reads, changes))
    }

    fn symbol(qualified_name: &str) -> SymbolId {
        SymbolId(format!("rift://symbol/rust/lib.rs/{qualified_name}"))
    }

    fn applied_summary(result: ChangeResult) -> rift_protocol::change::ChangeSummary {
        match result {
            ChangeResult::Applied { summary } => summary,
            ChangeResult::Refused { reason, .. } => {
                panic!("change must land, got refusal {reason:?}")
            }
        }
    }

    #[test]
    fn replace_symbol_rewrites_the_declaration_atomically() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: symbol("beacon"),
                region: None,
                body: "pub fn beacon() -> u8 {\n    7\n}".to_owned(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths, vec![ProjectPath("lib.rs".to_owned())]);
        assert_eq!(summary.edits.len(), 1);
        assert!(summary.diagnostics.is_empty(), "clean body parses cleanly");
        assert!(
            summary.id.0.len() == 8
                && summary
                    .id
                    .0
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "change id must be eight lowercase hex characters: {}",
            summary.id.0
        );
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(written, "pub fn beacon() -> u8 {\n    7\n}\n");
        Ok(())
    }

    /// A `typescript:tsx` symbol id minted by `get_symbol` round-trips
    /// through address parsing into an applied replacement, and the reparse
    /// runs through the tsx dialect.
    #[test]
    fn tsx_symbol_address_round_trips_from_minted_id_to_replacement() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("App.tsx"),
            "export function App() {\n  return <main>beacon</main>;\n}\n",
        )?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        let params: GetSymbolParams = serde_json::from_value(serde_json::json!({"name": "App"}))?;
        let hits = reads.get_symbol(&params)?.hits;
        let minted = hits[0].symbol.id.clone();
        assert_eq!(minted.0, "rift://symbol/typescript:tsx/App.tsx/App");
        let result = changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: minted,
                region: None,
                body: "function App() {\n  return <main>rift</main>;\n}".to_owned(),
            },
        )?;
        let summary = applied_summary(result);
        assert!(
            summary.diagnostics.is_empty(),
            "the tsx dialect reparses the replacement cleanly"
        );
        let written = fs::read_to_string(directory.path().join("App.tsx"))?;
        assert_eq!(
            written,
            "export function App() {\n  return <main>rift</main>;\n}\n"
        );
        Ok(())
    }

    /// A markdown symbol id minted by `get_symbol` - heading text escaped,
    /// nested under its ` > ` heading path - round-trips through address
    /// parsing into an applied section rewrite, and the reparse runs through
    /// the markdown provider.
    #[test]
    fn markdown_symbol_address_round_trips_from_minted_id_to_replacement() -> TestResult {
        let directory = tempfile::tempdir()?;
        let readme_md = "# Install\n\nIntro.\n\n## Requirements\n\n- a toolchain\n";
        fs::write(directory.path().join("README.md"), readme_md)?;
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let reads = ReadService::build(directory.path(), limits, &visibility, &inclusion, history)?;
        let changes = ChangeService::new(directory.path());
        let request = serde_json::json!({"name": "Requirements"});
        let params: GetSymbolParams = serde_json::from_value(request)?;
        let hits = reads.get_symbol(&params)?.hits;
        let minted = hits[0].symbol.id.clone();
        assert_eq!(
            minted.0,
            "rift://symbol/markdown/README.md/Install%20%3E%20Requirements"
        );
        let replacement = ReplaceSymbolParams {
            symbol: minted,
            region: None,
            body: "## Requirements\n\n- a newer toolchain\n".to_owned(),
        };
        let result = changes.replace_symbol(&reads, &replacement)?;
        let summary = applied_summary(result);
        assert!(
            summary.diagnostics.is_empty(),
            "the markdown provider reparses the rewritten section cleanly"
        );
        let written = fs::read_to_string(directory.path().join("README.md"))?;
        assert_eq!(
            written,
            "# Install\n\nIntro.\n\n## Requirements\n\n- a newer toolchain\n"
        );
        Ok(())
    }

    /// A JSON symbol id minted by `get_symbol` - key path escaped, nested
    /// under its ` > ` key path - round-trips through address parsing into
    /// an applied member rewrite, and the reparse runs through the JSON
    /// provider.
    #[test]
    fn json_symbol_address_round_trips_from_minted_id_to_replacement() -> TestResult {
        let directory = tempfile::tempdir()?;
        let settings_json = "{\"server\": {\"port\": 8080}}\n";
        fs::write(directory.path().join("settings.json"), settings_json)?;
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let reads = ReadService::build(directory.path(), limits, &visibility, &inclusion, history)?;
        let changes = ChangeService::new(directory.path());
        let request = serde_json::json!({"name": "port"});
        let params: GetSymbolParams = serde_json::from_value(request)?;
        let hits = reads.get_symbol(&params)?.hits;
        let minted = hits[0].symbol.id.clone();
        assert_eq!(
            minted.0,
            "rift://symbol/json/settings.json/server%20%3E%20port"
        );
        let replacement = ReplaceSymbolParams {
            symbol: minted,
            region: None,
            body: "\"port\": 9090".to_owned(),
        };
        let result = changes.replace_symbol(&reads, &replacement)?;
        let summary = applied_summary(result);
        assert!(
            summary.diagnostics.is_empty(),
            "the JSON provider reparses the rewritten member cleanly"
        );
        let written = fs::read_to_string(directory.path().join("settings.json"))?;
        assert_eq!(written, "{\"server\": {\"port\": 9090}}\n");
        Ok(())
    }

    #[test]
    fn replace_symbol_refuses_when_disk_drifted_from_snapshot() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() { }\n")?;
        let result = changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: symbol("beacon"),
                region: None,
                body: "pub fn beacon() -> u8 { 7 }".to_owned(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("drifted disk must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::SourceUnchanged
        );
        assert_ne!(preconditions[0].expected, preconditions[0].observed);
        let untouched = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(
            untouched, "pub fn beacon() { }\n",
            "refusal leaves the tree untouched"
        );
        Ok(())
    }

    #[test]
    fn concurrent_replacements_cannot_both_publish_from_one_snapshot() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let barrier = Arc::new(Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let first_barrier = Arc::clone(&barrier);
            let first_changes = &changes;
            let first_reads = &reads;
            let first = scope.spawn(move || {
                first_barrier.wait();
                first_changes.replace_symbol(
                    first_reads,
                    &ReplaceSymbolParams {
                        symbol: symbol("beacon"),
                        region: None,
                        body: "pub fn beacon() -> u8 { 1 }".to_owned(),
                    },
                )
            });
            let second = scope.spawn(|| {
                barrier.wait();
                changes.replace_symbol(
                    &reads,
                    &ReplaceSymbolParams {
                        symbol: symbol("beacon"),
                        region: None,
                        body: "pub fn beacon() -> u8 { 2 }".to_owned(),
                    },
                )
            });
            [
                first.join().expect("first change task must not panic"),
                second.join().expect("second change task must not panic"),
            ]
        });
        let [first, second] = results;
        let results = [first?, second?];
        let applied_count = results
            .iter()
            .filter(|result| matches!(result, ChangeResult::Applied { .. }))
            .count();
        let refused_count = results
            .iter()
            .filter(|result| matches!(result, ChangeResult::Refused { .. }))
            .count();
        assert_eq!(applied_count, 1, "one replacement must publish");
        assert_eq!(refused_count, 1, "stale replacement must refuse");
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert!(
            written.contains("-> u8 { 1 }") || written.contains("-> u8 { 2 }"),
            "published file must contain one complete replacement: {written}"
        );
        Ok(())
    }

    /// Two declarations spelling one name are suffixed apart, so no
    /// declaration holds the bare name: the bare address refuses as a
    /// missing target rather than landing silently on the first twin, and
    /// each suffixed address rewrites its own declaration.
    #[test]
    fn replace_symbol_refuses_the_bare_name_of_repeated_declarations() -> TestResult {
        const TWINS: &str =
            "#[cfg(unix)]\npub fn beacon() {}\n#[cfg(windows)]\npub fn beacon() {}\n";
        let (_directory, reads, changes) = fixture(TWINS)?;
        for absent in ["vanished", "beacon"] {
            let refused = changes.replace_symbol(
                &reads,
                &ReplaceSymbolParams {
                    symbol: symbol(absent),
                    region: None,
                    body: "pub fn replaced() {}".to_owned(),
                },
            )?;
            let ChangeResult::Refused {
                reason,
                preconditions,
                ..
            } = refused
            else {
                panic!("no declaration holds {absent}, so it must refuse");
            };
            assert_eq!(reason, RefusalReason::UnmetPrecondition);
            assert_eq!(
                preconditions[0].kind,
                OperationPreconditionKind::TargetExists
            );
            assert_eq!(
                preconditions[0].observed,
                PreconditionValue::Boolean { value: false }
            );
        }
        for (suffixed, twin, own) in [
            ("beacon~1", "#[cfg(windows)]", "#[cfg(unix)]"),
            ("beacon~2", "#[cfg(unix)]", "#[cfg(windows)]"),
        ] {
            let (directory, reads, changes) = fixture(TWINS)?;
            let params = ReplaceSymbolParams {
                symbol: symbol(suffixed),
                region: None,
                body: "pub fn beacon() -> u8 { 7 }".to_owned(),
            };
            let summary = applied_summary(changes.replace_symbol(&reads, &params)?);
            assert_eq!(
                summary.edits.len(),
                1,
                "{suffixed} rewrites one declaration"
            );
            let written = fs::read_to_string(directory.path().join("lib.rs"))?;
            assert!(written.contains("-> u8 { 7 }"), "{suffixed}: {written}");
            assert!(
                written.contains(twin),
                "{suffixed} keeps its twin: {written}"
            );
            assert!(
                !written.contains(own),
                "{suffixed} takes its own: {written}"
            );
        }
        Ok(())
    }

    #[test]
    fn insert_symbol_lands_on_the_requested_side() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: Some(symbol("beacon")),
                file: None,
                position: InsertPosition::Before,
                body: "/// Docs.\npub fn early() {}".to_owned(),
                create_missing: false,
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(
            written,
            "/// Docs.\npub fn early() {}\n\npub fn beacon() {}\n"
        );

        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let result = changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: Some(symbol("beacon")),
                file: None,
                position: InsertPosition::After,
                body: "pub fn late() {}".to_owned(),
                create_missing: false,
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(
            written,
            "/// Docs.\npub fn early() {}\n\npub fn beacon() {}\n\npub fn late() {}\n"
        );
        Ok(())
    }

    #[test]
    fn insert_symbol_before_a_documented_anchor_lands_above_its_doc_comment() -> TestResult {
        let (directory, reads, changes) =
            fixture("/// Beacon docs.\n#[derive(Debug)]\npub struct Beacon;\n")?;
        let result = changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: Some(symbol("Beacon")),
                file: None,
                position: InsertPosition::Before,
                body: "pub struct Early;".to_owned(),
                create_missing: false,
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(
            written,
            "pub struct Early;\n\n/// Beacon docs.\n#[derive(Debug)]\npub struct Beacon;\n",
            "the insertion must land above the doc comment, not between it and the struct"
        );
        Ok(())
    }

    #[test]
    fn replace_symbol_on_a_documented_declaration_leaves_no_orphaned_doc_or_attribute() -> TestResult
    {
        let (directory, reads, changes) =
            fixture("/// Old docs.\n#[derive(Debug)]\npub struct Beacon;\n")?;
        let result = changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: symbol("Beacon"),
                region: None,
                body: "/// New docs.\npub struct Beacon {\n    pub signal: u8,\n}".to_owned(),
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(
            written,
            "/// New docs.\npub struct Beacon {\n    pub signal: u8,\n}\n"
        );
        assert!(
            !written.contains("Old docs") && !written.contains("derive"),
            "the old doc comment and attribute must not survive the replacement: {written}"
        );
        Ok(())
    }

    #[test]
    fn insert_symbol_file_target_lands_at_the_requested_edge() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: None,
                file: Some(ProjectPath("lib.rs".to_owned())),
                position: InsertPosition::Before,
                body: "//! Module docs.".to_owned(),
                create_missing: false,
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(written, "//! Module docs.\n\npub fn beacon() {}\n");

        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let result = changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: None,
                file: Some(ProjectPath("lib.rs".to_owned())),
                position: InsertPosition::After,
                body: "pub fn tail() {}".to_owned(),
                create_missing: false,
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(
            written,
            "//! Module docs.\n\npub fn beacon() {}\n\n\npub fn tail() {}"
        );
        Ok(())
    }

    #[test]
    fn insert_symbol_creates_missing_file_with_nested_parent_directories() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: None,
                file: Some(ProjectPath("notes/todo/plan.rs".to_owned())),
                position: InsertPosition::After,
                body: "// plan".to_owned(),
                create_missing: true,
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("notes/todo/plan.rs"))?;
        assert_eq!(
            written, "// plan",
            "a created file's body lands exactly, with no anchor-insert spacing added"
        );
        Ok(())
    }

    #[test]
    fn insert_symbol_refuses_a_missing_file_without_create_missing() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: None,
                file: Some(ProjectPath("missing.rs".to_owned())),
                position: InsertPosition::After,
                body: "pub fn late() {}".to_owned(),
                create_missing: false,
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("a missing file target without create_missing must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::TargetExists
        );
        assert_eq!(
            preconditions[0].paths,
            vec![ProjectPath("missing.rs".to_owned())]
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn insert_symbol_file_target_surfaces_read_storage_failure() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let target = directory.path().join("lib.rs");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o000))?;
        let result = changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: None,
                file: Some(ProjectPath("lib.rs".to_owned())),
                position: InsertPosition::After,
                body: "pub fn late() {}".to_owned(),
                create_missing: false,
            },
        );
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644))?;
        let error = result.expect_err("an unreadable existing file must fail to insert");
        assert_eq!(error.descriptor().code(), "storage_failure");
        assert!(
            error.to_string().contains("operation read"),
            "failure must name the read operation: {error}"
        );
        Ok(())
    }

    #[test]
    fn insert_symbol_create_missing_on_an_existing_file_just_inserts() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: None,
                file: Some(ProjectPath("lib.rs".to_owned())),
                position: InsertPosition::After,
                body: "pub fn late() {}".to_owned(),
                create_missing: true,
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(written, "pub fn beacon() {}\n\n\npub fn late() {}");
        Ok(())
    }

    #[test]
    fn insert_symbol_file_target_accepts_a_non_rust_extension() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: None,
                file: Some(ProjectPath("notes/TODO.txt".to_owned())),
                position: InsertPosition::Before,
                body: "- write docs".to_owned(),
                create_missing: true,
            },
        )?;
        let summary = applied_summary(result);
        assert!(
            summary.diagnostics.is_empty(),
            "no provider claims txt, so the change reports no reparse findings"
        );
        let written = fs::read_to_string(directory.path().join("notes/TODO.txt"))?;
        assert_eq!(written, "- write docs");
        Ok(())
    }

    #[test]
    fn insert_symbol_rejects_invalid_target_combinations() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let both = changes
            .insert_symbol(
                &reads,
                &InsertSymbolParams {
                    anchor: Some(symbol("beacon")),
                    file: Some(ProjectPath("lib.rs".to_owned())),
                    position: InsertPosition::After,
                    body: "pub fn late() {}".to_owned(),
                    create_missing: false,
                },
            )
            .expect_err("both anchor and file must be rejected");
        assert_eq!(both.descriptor().code(), "invalid_request");

        let neither = changes
            .insert_symbol(
                &reads,
                &InsertSymbolParams {
                    anchor: None,
                    file: None,
                    position: InsertPosition::After,
                    body: "pub fn late() {}".to_owned(),
                    create_missing: false,
                },
            )
            .expect_err("a request naming neither target must be rejected");
        assert_eq!(neither.descriptor().code(), "invalid_request");

        let create_missing_with_anchor = changes
            .insert_symbol(
                &reads,
                &InsertSymbolParams {
                    anchor: Some(symbol("beacon")),
                    file: None,
                    position: InsertPosition::After,
                    body: "pub fn late() {}".to_owned(),
                    create_missing: true,
                },
            )
            .expect_err("create_missing with an anchor target must be rejected");
        assert_eq!(
            create_missing_with_anchor.descriptor().code(),
            "invalid_request"
        );
        Ok(())
    }

    #[test]
    fn insert_symbol_file_target_rejects_rift_state_paths() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let error = changes
            .insert_symbol(
                &reads,
                &InsertSymbolParams {
                    anchor: None,
                    file: Some(ProjectPath(".rift/x.rs".to_owned())),
                    position: InsertPosition::After,
                    body: "x".to_owned(),
                    create_missing: true,
                },
            )
            .expect_err("a .rift-state file target must be rejected");
        assert_eq!(error.descriptor().code(), "invalid_request");
        Ok(())
    }

    /// `insert_at_file` resolves through the write gate before reading or
    /// writing, for both the existing-file and the `create_missing` arms.
    #[test]
    fn insert_symbol_file_target_refuses_into_an_excluded_directory() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let visibility = SourceVisibility::new(Vec::new(), vec!["excluded/**".to_owned()], true);
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        for create_missing in [false, true] {
            let result = changes.insert_symbol(
                &reads,
                &InsertSymbolParams {
                    anchor: None,
                    file: Some(ProjectPath("excluded/notes.rs".to_owned())),
                    position: InsertPosition::After,
                    body: "pub fn late() {}".to_owned(),
                    create_missing,
                },
            )?;
            let ChangeResult::Refused {
                reason,
                diagnostics,
                ..
            } = result
            else {
                panic!("an excluded destination must refuse, create_missing={create_missing}");
            };
            assert_eq!(
                reason,
                RefusalReason::Unsupported,
                "a policy-excluded target refuses as unsupported, not unmet_precondition \
                 (create_missing={create_missing})"
            );
            assert!(
                diagnostics.iter().any(|diagnostic| {
                    diagnostic.message.contains("excluded/notes.rs")
                        && diagnostic.message.contains("[source]")
                }),
                "the diagnostic must name the excluded path and the policy \
                 (create_missing={create_missing}): {diagnostics:?}"
            );
        }
        assert!(
            !directory.path().join("excluded").exists(),
            "a refused insert must leave the tree untouched"
        );
        Ok(())
    }

    #[test]
    fn replace_node_verifies_its_witness_both_ways() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let listing = reads.nodes(NodesParams {
            path: ProjectPath("lib.rs".to_owned()),
            position: 3,
            projection: None,
            rev: None,
        })?;
        let address = listing.nodes[0].id.0.clone();

        let mut stale = address.clone();
        stale.replace_range(stale.len() - 8.., "00000000");
        let refused = changes.replace_node(
            &reads,
            &ReplaceNodeParams {
                node: NodeId(stale),
                region: None,
                body: "pub fn beacon() -> u8 { 7 }".to_owned(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = refused
        else {
            panic!("stale witness must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::SourceUnchanged
        );
        assert_eq!(
            preconditions[0].expected,
            PreconditionValue::Text {
                value: "00000000".to_owned()
            }
        );

        let applied = changes.replace_node(
            &reads,
            &ReplaceNodeParams {
                node: NodeId(address),
                region: None,
                body: "pub fn beacon() -> u8 {\n    7\n}".to_owned(),
            },
        )?;
        applied_summary(applied);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert!(written.contains("-> u8"));
        Ok(())
    }

    #[test]
    fn broken_body_lands_and_reports_parser_findings() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: symbol("beacon"),
                region: None,
                body: "pub fn beacon( {".to_owned(),
            },
        )?;
        let summary = applied_summary(result);
        assert!(
            !summary.diagnostics.is_empty(),
            "a body that breaks the parse must say so on the result"
        );
        Ok(())
    }

    #[test]
    fn region_scoped_replacement_is_not_served_yet() -> TestResult {
        let (_directory, reads, changes) = fixture(
            "pub fn beacon() {}
",
        )?;
        let error = changes
            .replace_symbol(
                &reads,
                &ReplaceSymbolParams {
                    symbol: symbol("beacon"),
                    region: Some(rift_protocol::read::RegionRole::Body),
                    body: "7".to_owned(),
                },
            )
            .expect_err("region replacement must be refused as unserved");
        assert_eq!(error.descriptor().code(), "capability_unavailable");
        Ok(())
    }

    #[test]
    fn malformed_addresses_fail_before_resolution() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let symbol_error = changes
            .replace_symbol(
                &reads,
                &ReplaceSymbolParams {
                    symbol: SymbolId("not-an-address".to_owned()),
                    region: None,
                    body: "x".to_owned(),
                },
            )
            .expect_err("malformed symbol address must error");
        assert_eq!(symbol_error.descriptor().code(), "invalid_request");
        let node_error = changes
            .replace_node(
                &reads,
                &ReplaceNodeParams {
                    node: NodeId("rift://node/rust/lib.rs@9-3#zzzzzzzz".to_owned()),
                    region: None,
                    body: "x".to_owned(),
                },
            )
            .expect_err("inverted span must error");
        assert_eq!(node_error.descriptor().code(), "invalid_request");
        Ok(())
    }

    #[test]
    fn replace_symbol_refuses_when_the_addressed_file_is_unindexed() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: SymbolId("rift://symbol/rust/ghost.rs/beacon".to_owned()),
                region: None,
                body: "pub fn beacon() {}".to_owned(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("unindexed file must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::TargetExists
        );
        assert_eq!(
            preconditions[0].paths,
            vec![ProjectPath("ghost.rs".to_owned())]
        );
        Ok(())
    }

    #[test]
    fn insert_symbol_refusal_names_the_missing_anchor_address() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: Some(symbol("vanished")),
                file: None,
                position: InsertPosition::After,
                body: "pub fn late() {}".to_owned(),
                create_missing: false,
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("missing anchor must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::TargetExists
        );
        assert_eq!(
            preconditions[0].addresses,
            vec![PreconditionAddress::Symbol {
                symbol: symbol("vanished")
            }]
        );
        Ok(())
    }

    #[test]
    fn symbol_address_with_invalid_path_reports_the_violation() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let error = changes
            .replace_symbol(
                &reads,
                &ReplaceSymbolParams {
                    symbol: SymbolId("rift://symbol/rust/%2Fetc%2Fpasswd/beacon".to_owned()),
                    region: None,
                    body: "x".to_owned(),
                },
            )
            .expect_err("absolute path inside a symbol address must error");
        assert_eq!(error.descriptor().code(), "invalid_request");
        assert!(
            error.to_string().contains("violation"),
            "message must name the broken rule: {error}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn readonly_workspace_surfaces_stage_storage_failure() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o555))?;
        let result = changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: symbol("beacon"),
                region: None,
                body: "pub fn beacon() -> u8 { 7 }".to_owned(),
            },
        );
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755))?;
        let error = result.expect_err("read-only tree must fail to stage");
        assert_eq!(error.descriptor().code(), "storage_failure");
        assert!(
            error.to_string().contains("operation stage"),
            "failure must name the stage operation: {error}"
        );
        let untouched = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(untouched, "pub fn beacon() {}\n");
        Ok(())
    }

    #[test]
    fn oversized_replacement_lands_but_reports_reparse_bound() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let body = format!(
            "pub fn beacon() {{}}\n// {}",
            "x".repeat(rift_syntax::RustSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT)
        );
        let result = changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: symbol("beacon"),
                region: None,
                body,
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.diagnostics.len(), 1);
        assert!(
            summary.diagnostics[0]
                .message
                .contains("no longer parses within bounds"),
            "diagnostic must name the crossed bound: {}",
            summary.diagnostics[0].message
        );
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert!(written.len() > rift_syntax::RustSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT);
        Ok(())
    }

    #[test]
    fn replace_node_refuses_region_scope_as_unserved() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let error = changes
            .replace_node(
                &reads,
                &ReplaceNodeParams {
                    node: NodeId("rift://node/rust/lib.rs@0-18#aaaaaaaa".to_owned()),
                    region: Some(rift_protocol::read::RegionRole::Body),
                    body: "7".to_owned(),
                },
            )
            .expect_err("region replacement must be refused as unserved");
        assert_eq!(error.descriptor().code(), "capability_unavailable");
        Ok(())
    }

    #[test]
    fn replace_node_refusal_names_the_missing_file_by_node_address() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let node = NodeId("rift://node/rust/ghost.rs@0-5#aaaaaaaa".to_owned());
        let result = changes.replace_node(
            &reads,
            &ReplaceNodeParams {
                node: node.clone(),
                region: None,
                body: "pub fn beacon() {}".to_owned(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("unindexed file must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::TargetExists
        );
        assert_eq!(
            preconditions[0].addresses,
            vec![PreconditionAddress::Node { node }]
        );
        assert_eq!(
            preconditions[0].paths,
            vec![ProjectPath("ghost.rs".to_owned())]
        );
        Ok(())
    }

    #[test]
    fn node_address_with_invalid_path_reports_the_violation() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let error = changes
            .replace_node(
                &reads,
                &ReplaceNodeParams {
                    node: NodeId("rift://node/rust/%2Fetc%2Fpasswd@0-5#aaaaaaaa".to_owned()),
                    region: None,
                    body: "x".to_owned(),
                },
            )
            .expect_err("absolute path inside a node address must error");
        assert_eq!(error.descriptor().code(), "invalid_request");
        assert!(
            error.to_string().contains("violation"),
            "message must name the broken rule: {error}"
        );
        Ok(())
    }

    #[test]
    fn symbol_address_round_trips_the_rust_segment_through_mint_and_parse() -> TestResult {
        let minted = rift_core::symbol_identity("rust", "lib.rs", "beacon");
        assert_eq!(minted, symbol("beacon").0);
        let parsed = super::parse_symbol_address(&minted)?;
        assert_eq!(parsed.language_segment, "rust");
        assert_eq!(parsed.path.as_str(), "lib.rs");
        assert_eq!(parsed.qualified_name, "beacon");
        Ok(())
    }

    #[test]
    fn symbol_address_round_trips_a_dialect_segment_through_mint_and_parse() -> TestResult {
        let minted = rift_core::symbol_identity("typescript:tsx", "src/App.tsx", "render");
        let parsed = super::parse_symbol_address(&minted)?;
        assert_eq!(parsed.language_segment, "typescript:tsx");
        assert_eq!(parsed.path.as_str(), "src/App.tsx");
        assert_eq!(parsed.qualified_name, "render");
        Ok(())
    }

    #[test]
    fn node_address_parses_the_language_segment_for_name_and_dialect_forms() -> TestResult {
        let rust: super::NodeAddress = "rift://node/rust/lib.rs@0-4#aaaaaaaa".parse()?;
        assert_eq!(rust.language_segment, "rust");
        assert_eq!(rust.path.as_str(), "lib.rs");
        let dialect: super::NodeAddress =
            "rift://node/typescript:tsx/src/App.tsx@0-4#aaaaaaaa".parse()?;
        assert_eq!(dialect.language_segment, "typescript:tsx");
        assert_eq!(dialect.path.as_str(), "src/App.tsx");
        Ok(())
    }

    #[test]
    fn addresses_with_an_empty_language_segment_are_malformed() {
        let symbol_error = super::parse_symbol_address("rift://symbol//lib.rs/beacon")
            .expect_err("an empty language segment must be malformed");
        assert_eq!(symbol_error.descriptor().code(), "invalid_request");
        let node_error = "rift://node//lib.rs@0-4#aaaaaaaa"
            .parse::<super::NodeAddress>()
            .expect_err("an empty language segment must be malformed");
        assert_eq!(node_error.descriptor().code(), "invalid_request");
    }

    #[test]
    fn replace_symbol_refuses_an_address_language_that_mismatches_the_document() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let error = changes
            .replace_symbol(
                &reads,
                &ReplaceSymbolParams {
                    symbol: SymbolId("rift://symbol/typescript/lib.rs/beacon".to_owned()),
                    region: None,
                    body: "pub fn beacon() {}".to_owned(),
                },
            )
            .expect_err("a mismatched address language must be refused as invalid");
        assert_eq!(error.descriptor().code(), "invalid_request");
        assert!(
            error
                .to_string()
                .contains("address language typescript does not match the indexed language rust"),
            "message must name both languages: {error}"
        );
        Ok(())
    }

    #[test]
    fn replace_node_refuses_an_address_language_that_mismatches_the_document() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let error = changes
            .replace_node(
                &reads,
                &ReplaceNodeParams {
                    node: NodeId("rift://node/typescript/lib.rs@0-5#aaaaaaaa".to_owned()),
                    region: None,
                    body: "x".to_owned(),
                },
            )
            .expect_err("a mismatched address language must be refused as invalid");
        assert_eq!(error.descriptor().code(), "invalid_request");
        assert!(
            error
                .to_string()
                .contains("address language typescript does not match the indexed language rust"),
            "message must name both languages: {error}"
        );
        Ok(())
    }

    #[test]
    fn reparse_skips_a_path_no_provider_claims() -> TestResult {
        let path = CoreProjectPath::new("notes/TODO.txt")?;
        let unit = FileId("rift://file/notes/TODO.txt".to_owned());
        let diagnostics = super::reparse_diagnostics(unit, &path, "fn broken( {");
        assert!(
            diagnostics.is_empty(),
            "an unclaimed extension has no grammar, so no findings"
        );
        Ok(())
    }

    #[test]
    fn reparse_stamps_findings_with_the_claiming_provider_language() -> TestResult {
        let path = CoreProjectPath::new("lib.rs")?;
        let unit = FileId("rift://file/lib.rs".to_owned());
        let diagnostics = super::reparse_diagnostics(unit, &path, "fn broken( {");
        assert!(!diagnostics.is_empty(), "broken rust must report findings");
        assert_eq!(
            diagnostics[0].language,
            Some(Language {
                name: "rust".to_owned(),
                dialect: None,
            })
        );
        Ok(())
    }

    #[test]
    fn replace_node_span_beyond_the_file_fails_as_invalid() -> TestResult {
        let source = "pub fn beacon() {}\n";
        let (directory, reads, changes) = fixture(source)?;
        let end = source.len() as u64 + 10;
        let range = ByteRange { start: 0, end };
        let witness = node_witness(source, range);
        let error = changes
            .replace_node(
                &reads,
                &ReplaceNodeParams {
                    node: NodeId(format!("rift://node/rust/lib.rs@0-{end}#{witness}")),
                    region: None,
                    body: "pub fn beacon() -> u8 { 7 }".to_owned(),
                },
            )
            .expect_err("a span past the file end must error");
        assert_eq!(error.descriptor().code(), "invalid_request");
        assert!(
            error.to_string().contains("outside the addressed file"),
            "message must name the span fault: {error}"
        );
        let untouched = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(untouched, source);
        Ok(())
    }

    /// Builds a workspace of several files and the services over it.
    fn multi_file_fixture(
        files: &[(&str, &str)],
    ) -> TestResult<(tempfile::TempDir, ReadService, ChangeService)> {
        let directory = tempfile::tempdir()?;
        for (name, source) in files {
            fs::write(directory.path().join(name), source)?;
        }
        let root = directory.path();
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let inclusion = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let reads = ReadService::build(root, limits, &visibility, &inclusion, history)?;
        let changes = ChangeService::new(root);
        Ok((directory, reads, changes))
    }

    /// The single region two images differ over, standing in for the engine
    /// edits a real plan carries. Fixture sources are ASCII, so the byte
    /// scan lands on character boundaries.
    fn differing_region(base: &str, next: &str) -> ReplacedRegion {
        let prefix = base
            .bytes()
            .zip(next.bytes())
            .take_while(|(left, right)| left == right)
            .count();
        let suffix = base[prefix..]
            .bytes()
            .rev()
            .zip(next[prefix..].bytes().rev())
            .take_while(|(left, right)| left == right)
            .count();
        ReplacedRegion {
            range: ByteRange {
                start: prefix as u64,
                end: (base.len() - suffix) as u64,
            },
            text: next[prefix..next.len() - suffix].to_owned(),
        }
    }

    fn rename_plan(rewrites: Vec<(&str, &str, &str)>) -> crate::rename::RenamePlan {
        crate::rename::RenamePlan {
            symbol: SymbolId("rift://symbol/rust/lib.rs/beacon".to_owned()),
            old_name: "beacon".to_owned(),
            rewrites: rewrites
                .into_iter()
                .map(
                    |(path, base_source, next_source)| crate::rename::PlannedRewrite {
                        path: CoreProjectPath::new(path).expect("fixture path is valid"),
                        base_source: base_source.to_owned(),
                        next_source: next_source.to_owned(),
                        replaced: vec![differing_region(base_source, next_source)],
                    },
                )
                .collect(),
        }
    }

    #[test]
    fn apply_rename_writes_every_file_and_sweeps_clean() -> TestResult {
        let library = "pub fn beacon() {}\n";
        let caller = "pub fn caller() { beacon(); }\n";
        let (directory, reads, changes) =
            multi_file_fixture(&[("lib.rs", library), ("main.rs", caller)])?;
        let plan = rename_plan(vec![
            ("lib.rs", library, "pub fn flare() {}\n"),
            ("main.rs", caller, "pub fn caller() { flare(); }\n"),
        ]);
        let summary = applied_summary(changes.apply_rename(&reads, &plan)?);
        assert_eq!(summary.paths.len(), 2);
        assert_eq!(summary.edits.len(), 2, "one edit per replaced region");
        let Edit::Replace { span, text } = &summary.edits[0];
        assert_eq!(
            (span.range.start, span.range.end),
            (7, 13),
            "the edit names the renamed identifier, not the whole file"
        );
        assert_eq!(text, "flare");
        assert!(
            summary
                .diagnostics
                .iter()
                .all(|finding| finding.code.as_deref() != Some("rift.rename.survivor")),
            "a full rename must sweep clean: {:?}",
            summary.diagnostics
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("lib.rs"))?,
            "pub fn flare() {}\n"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("main.rs"))?,
            "pub fn caller() { flare(); }\n"
        );
        Ok(())
    }

    #[test]
    fn apply_rename_reports_survivors_after_a_partial_rewrite() -> TestResult {
        let library = "pub fn beacon() {}\n";
        let caller = "pub fn caller() { beacon(); }\n";
        let (_directory, reads, changes) =
            multi_file_fixture(&[("lib.rs", library), ("main.rs", caller)])?;
        let plan = rename_plan(vec![("lib.rs", library, "pub fn flare() {}\n")]);
        let summary = applied_summary(changes.apply_rename(&reads, &plan)?);
        let survivor = summary
            .diagnostics
            .iter()
            .find(|finding| finding.code.as_deref() == Some("rift.rename.survivor"))
            .expect("the unrewritten caller must surface as a survivor");
        assert!(survivor.message.contains("beacon"));
        Ok(())
    }

    #[test]
    fn apply_rename_refuses_when_the_disk_drifted_after_planning() -> TestResult {
        let library = "pub fn beacon() {}\n";
        let (directory, reads, changes) = multi_file_fixture(&[("lib.rs", library)])?;
        let plan = rename_plan(vec![("lib.rs", library, "pub fn flare() {}\n")]);
        let drifted = "pub fn beacon() { let _late = 1; }\n";
        fs::write(directory.path().join("lib.rs"), drifted)?;
        let result = changes.apply_rename(&reads, &plan)?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("a drifted base must refuse, got {result:?}");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::SourceUnchanged
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("lib.rs"))?,
            drifted,
            "a refusal leaves the tree untouched"
        );
        Ok(())
    }

    #[test]
    fn apply_rename_refuses_when_a_planned_file_left_the_index() -> TestResult {
        let library = "pub fn beacon() {}\n";
        let (_directory, reads, changes) = multi_file_fixture(&[("lib.rs", library)])?;
        let plan = rename_plan(vec![("vanished.rs", library, "pub fn flare() {}\n")]);
        let result = changes.apply_rename(&reads, &plan)?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("an unindexed file must refuse, got {result:?}");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::TargetExists
        );
        Ok(())
    }

    /// A move plan over the fixture's `old/hub.rs`, with an optional
    /// reference rewrite and an optional reason its references were not
    /// updated.
    fn move_plan(
        from: &str,
        to: &str,
        moved: (&str, &str),
        rewrites: Vec<(&str, &str, &str)>,
        references_not_updated: Option<crate::move_file::ReferencesNotUpdated>,
    ) -> crate::move_file::MovePlan {
        crate::move_file::MovePlan {
            from: CoreProjectPath::new(from).expect("fixture path is valid"),
            to: CoreProjectPath::new(to).expect("fixture path is valid"),
            moved_source: moved.0.to_owned(),
            moved_next: moved.1.to_owned(),
            rewrites: rewrites
                .into_iter()
                .map(
                    |(path, base_source, next_source)| crate::rename::PlannedRewrite {
                        path: CoreProjectPath::new(path).expect("fixture path is valid"),
                        base_source: base_source.to_owned(),
                        next_source: next_source.to_owned(),
                        replaced: vec![differing_region(base_source, next_source)],
                    },
                )
                .collect(),
            references_not_updated,
        }
    }

    #[test]
    fn apply_move_lands_the_move_and_reference_rewrites_atomically() -> TestResult {
        let hub = "pub fn hub() {}\n// hub module\n";
        let caller = "mod hub;\n";
        let (directory, reads, changes) =
            multi_file_fixture(&[("hub.rs", hub), ("main.rs", caller)])?;
        let plan = move_plan(
            "hub.rs",
            "spoke.rs",
            (hub, "pub fn hub() {}\n// spoke module\n"),
            vec![("main.rs", caller, "mod spoke;\n")],
            None,
        );
        let summary = applied_summary(changes.apply_move(&reads, &plan)?);
        assert_eq!(
            summary.paths,
            vec![
                ProjectPath("hub.rs".to_owned()),
                ProjectPath("main.rs".to_owned()),
                ProjectPath("spoke.rs".to_owned()),
            ],
            "the summary carries the old path, the rewrite, and the new path, sorted"
        );
        assert_eq!(summary.edits.len(), 3);
        assert!(
            summary
                .diagnostics
                .iter()
                .all(|finding| finding.code.as_deref() != Some("rift.move.references_not_updated")),
            "an engine-covered move carries no skip warning: {:?}",
            summary.diagnostics
        );
        assert!(!directory.path().join("hub.rs").exists());
        assert_eq!(
            fs::read_to_string(directory.path().join("spoke.rs"))?,
            "pub fn hub() {}\n// spoke module\n",
            "the engine's edit to the moved file lands at the destination"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("main.rs"))?,
            "mod spoke;\n"
        );
        Ok(())
    }

    #[test]
    fn apply_move_into_a_new_directory_creates_its_parents() -> TestResult {
        let hub = "pub fn hub() {}\n";
        let (directory, reads, changes) = multi_file_fixture(&[("hub.rs", hub)])?;
        let reason = crate::move_file::ReferencesNotUpdated::NoEngine {
            language_segment: "rust".to_owned(),
        };
        let plan = move_plan(
            "hub.rs",
            "nested/deep/hub.rs",
            (hub, hub),
            vec![],
            Some(reason),
        );
        let summary = applied_summary(changes.apply_move(&reads, &plan)?);
        let warning = summary
            .diagnostics
            .iter()
            .find(|finding| finding.code.as_deref() == Some("rift.move.references_not_updated"))
            .expect("a skipped engine must surface as the warning");
        assert_eq!(warning.severity, rift_protocol::read::Severity::Warning);
        assert_eq!(
            fs::read_to_string(directory.path().join("nested/deep/hub.rs"))?,
            hub,
            "missing parent directories are created on publish"
        );
        assert!(!directory.path().join("hub.rs").exists());
        Ok(())
    }

    #[test]
    fn apply_move_refuses_when_the_destination_appeared_after_planning() -> TestResult {
        let hub = "pub fn hub() {}\n";
        let (directory, reads, changes) = multi_file_fixture(&[("hub.rs", hub)])?;
        let plan = move_plan("hub.rs", "spoke.rs", (hub, hub), vec![], None);
        fs::write(directory.path().join("spoke.rs"), "pub fn late() {}\n")?;
        let result = changes.apply_move(&reads, &plan)?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("an occupied destination must refuse, got {result:?}");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::TargetExists
        );
        assert_eq!(
            preconditions[0].expected,
            PreconditionValue::Boolean { value: false }
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("hub.rs"))?,
            hub,
            "a refusal leaves the tree untouched"
        );
        Ok(())
    }

    #[test]
    fn apply_move_refuses_when_the_moved_bytes_drifted_after_planning() -> TestResult {
        let hub = "pub fn hub() {}\n";
        let (directory, reads, changes) = multi_file_fixture(&[("hub.rs", hub)])?;
        let plan = move_plan("hub.rs", "spoke.rs", (hub, hub), vec![], None);
        let drifted = "pub fn hub() { let _late = 1; }\n";
        fs::write(directory.path().join("hub.rs"), drifted)?;
        let result = changes.apply_move(&reads, &plan)?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("drifted moved bytes must refuse, got {result:?}");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::SourceUnchanged
        );
        assert!(!directory.path().join("spoke.rs").exists());
        Ok(())
    }

    /// A publish failure in the middle of a move restores the original
    /// tree: the destination's created file is removed and every rewrite
    /// already published is restored from the index. The sealed source
    /// directory makes the source's removal - the last publish in path
    /// order here - fail deterministically.
    #[cfg(unix)]
    #[test]
    fn apply_move_mid_publish_failure_restores_the_original_tree() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        let hub = "pub fn hub() {}\n";
        let caller = "mod hub;\n";
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("old"))?;
        fs::write(directory.path().join("old/hub.rs"), hub)?;
        fs::write(directory.path().join("main.rs"), caller)?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        let plan = move_plan(
            "old/hub.rs",
            "hub.rs",
            (hub, hub),
            vec![("main.rs", caller, "mod moved_hub;\n")],
            None,
        );
        fs::set_permissions(
            directory.path().join("old"),
            fs::Permissions::from_mode(0o555),
        )?;
        let result = changes.apply_move(&reads, &plan);
        fs::set_permissions(
            directory.path().join("old"),
            fs::Permissions::from_mode(0o755),
        )?;
        let error = result.expect_err("the sealed source directory must fail the publish");
        assert_eq!(error.descriptor().code(), "storage_failure");
        assert!(
            error.to_string().contains("operation publish"),
            "failure must name the publish operation: {error}"
        );
        assert!(
            !directory.path().join("hub.rs").exists(),
            "the created destination is rolled back"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("main.rs"))?,
            caller,
            "the published rewrite is restored from the index"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("old/hub.rs"))?,
            hub,
            "the source never left"
        );
        Ok(())
    }

    #[test]
    fn patch_against_a_multi_hunk_file_reports_one_edit_per_hunk_at_its_own_span() -> TestResult {
        let filler = "// pad\n".repeat(50);
        let source = format!("one();\ntwo();\nthree();\nfour();\nfive();\n{filler}");
        let (_directory, reads, changes) = fixture(&source)?;
        let patch =
            "--- a/lib.rs\n+++ b/lib.rs\n@@ -2 +2 @@\n-two();\n+TWO();\n@@ -4 +4 @@\n-four();\n+FOUR();\n"
                .to_owned();
        let summary = applied_summary(changes.patch(&reads, &PatchParams { patch })?);
        assert_eq!(
            summary.edits.len(),
            2,
            "one edit per hunk, not one edit for the whole file"
        );
        let Edit::Replace {
            span: first_span,
            text: first_text,
        } = &summary.edits[0];
        assert_eq!(
            (first_span.range.start, first_span.range.end),
            (7, 14),
            "the first hunk's span is its own `two();\\n` line, not the file"
        );
        assert_eq!(first_text, "TWO();\n");
        let Edit::Replace {
            span: second_span,
            text: second_text,
        } = &summary.edits[1];
        assert_eq!((second_span.range.start, second_span.range.end), (23, 31));
        assert_eq!(second_text, "FOUR();\n");
        let total_edit_bytes: usize = summary
            .edits
            .iter()
            .map(|edit| {
                let Edit::Replace { text, .. } = edit;
                text.len()
            })
            .sum();
        assert!(
            total_edit_bytes < source.len() / 10,
            "edits ({total_edit_bytes} bytes) must be far smaller than the file they patch \
             ({} bytes)",
            source.len()
        );
        Ok(())
    }

    #[test]
    fn patch_creates_a_file_and_reports_one_edit_at_the_empty_range() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = "--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1 @@\n+pub fn fresh() {}\n".to_owned();
        let summary = applied_summary(changes.patch(&reads, &PatchParams { patch })?);
        assert_eq!(summary.edits.len(), 1);
        let Edit::Replace { span, text } = &summary.edits[0];
        assert_eq!(
            (span.range.start, span.range.end),
            (0, 0),
            "a created file has no previous image, so its edit spans the empty range"
        );
        assert_eq!(text, "pub fn fresh() {}\n");
        Ok(())
    }

    #[test]
    fn patch_deletes_a_file_and_reports_one_edit_across_the_whole_previous_image() -> TestResult {
        let source = "pub fn beacon() {}\npub fn steady() {}\n";
        let (_directory, reads, changes) = fixture(source)?;
        let patch = [
            "--- a/lib.rs",
            "+++ /dev/null",
            "@@ -1,2 +0,0 @@",
            "-pub fn beacon() {}",
            "-pub fn steady() {}",
            "",
        ]
        .join("\n");
        let summary = applied_summary(changes.patch(&reads, &PatchParams { patch })?);
        assert_eq!(summary.edits.len(), 1);
        let Edit::Replace { span, text } = &summary.edits[0];
        assert_eq!(
            (span.range.start, span.range.end),
            (0, source.len() as u64),
            "a deleted file's edit spans its whole previous image"
        );
        assert_eq!(text, "");
        Ok(())
    }

    /// Staging used to land at `absolute.with_extension("rift-staged")`,
    /// so a workspace file already named `notes.rift-staged` sat exactly
    /// where a change to `notes.rs` staged its own next content. The
    /// exclusive tempfile staging never touches a real workspace path
    /// until publish, so this file survives untouched.
    #[test]
    fn change_to_a_file_does_not_touch_a_workspace_file_literally_named_rift_staged() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("notes.rs"), "pub fn beacon() {}\n")?;
        fs::write(directory.path().join("notes.rift-staged"), "sentinel\n")?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        let patch = "--- a/notes.rs\n+++ b/notes.rs\n@@ -1 +1 @@\n-pub fn beacon() {}\n+pub fn renamed() {}\n"
            .to_owned();
        applied_summary(changes.patch(&reads, &PatchParams { patch })?);
        assert_eq!(
            fs::read_to_string(directory.path().join("notes.rs"))?,
            "pub fn renamed() {}\n"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("notes.rift-staged"))?,
            "sentinel\n",
            "a workspace file literally named notes.rift-staged must be untouched"
        );
        Ok(())
    }

    #[test]
    fn insert_symbol_file_target_before_reports_a_zero_width_edit_at_the_start() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let params = InsertSymbolParams {
            anchor: None,
            file: Some(ProjectPath("lib.rs".to_owned())),
            position: InsertPosition::Before,
            body: "//! Module docs.".to_owned(),
            create_missing: false,
        };
        let summary = applied_summary(changes.insert_symbol(&reads, &params)?);
        assert_eq!(
            summary.edits.len(),
            1,
            "a file-target insert reports one edit, not the whole file"
        );
        let Edit::Replace { span, text } = &summary.edits[0];
        assert_eq!(
            (span.range.start, span.range.end),
            (0, 0),
            "a `before` insert at a file target lands at the empty range at byte 0"
        );
        assert_eq!(text, "//! Module docs.\n\n");
        Ok(())
    }

    #[test]
    fn insert_symbol_file_target_after_reports_a_zero_width_edit_at_the_end() -> TestResult {
        let source = "pub fn beacon() {}\n";
        let (_directory, reads, changes) = fixture(source)?;
        let params = InsertSymbolParams {
            anchor: None,
            file: Some(ProjectPath("lib.rs".to_owned())),
            position: InsertPosition::After,
            body: "pub fn late() {}".to_owned(),
            create_missing: false,
        };
        let summary = applied_summary(changes.insert_symbol(&reads, &params)?);
        assert_eq!(summary.edits.len(), 1);
        let Edit::Replace { span, text } = &summary.edits[0];
        let end = source.len() as u64;
        assert_eq!(
            (span.range.start, span.range.end),
            (end, end),
            "an `after` insert at a file target lands at the empty range at the file's own end"
        );
        assert_eq!(text, "\n\npub fn late() {}");
        Ok(())
    }

    #[test]
    fn apply_rename_batch_past_the_edit_bound_falls_back_to_one_whole_file_edit() -> TestResult {
        let region_count = super::CHANGE_EDITS_MAX + 1;
        let base_source = "x\n".repeat(region_count);
        let next_source = "y\n".repeat(region_count);
        let (directory, reads, changes) = multi_file_fixture(&[("lib.rs", base_source.as_str())])?;
        let replaced: Vec<ReplacedRegion> = (0..region_count)
            .map(|index| {
                let start = (index * 2) as u64;
                ReplacedRegion {
                    range: ByteRange {
                        start,
                        end: start + 2,
                    },
                    text: "y\n".to_owned(),
                }
            })
            .collect();
        let rewrite = crate::rename::PlannedRewrite {
            path: CoreProjectPath::new("lib.rs")?,
            base_source: base_source.clone(),
            next_source: next_source.clone(),
            replaced,
        };
        let plan = crate::rename::RenamePlan {
            symbol: SymbolId("rift://symbol/rust/lib.rs/beacon".to_owned()),
            old_name: "beacon".to_owned(),
            rewrites: vec![rewrite],
        };
        let summary = applied_summary(changes.apply_rename(&reads, &plan)?);
        assert_eq!(
            summary.edits.len(),
            1,
            "{region_count} regions exceed CHANGE_EDITS_MAX, so the batch must fall back to one \
             whole-file edit"
        );
        let Edit::Replace { span, text } = &summary.edits[0];
        assert_eq!(
            (span.range.start, span.range.end),
            (0, base_source.len() as u64),
            "the fallback edit spans the whole previous image"
        );
        assert_eq!(text, &next_source);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(written, next_source);
        Ok(())
    }

    /// A `Modify` rewrite of `lib.rs` carrying `count` non-overlapping,
    /// single-byte regions, standing in for a batch whose region count alone
    /// decides whether it fits the edit bound.
    fn rewrite_with_region_count(count: usize) -> TestResult<FileRewrite> {
        let replaced: Vec<ReplacedRegion> = (0..count)
            .map(|index| ReplacedRegion {
                range: ByteRange {
                    start: index as u64,
                    end: index as u64 + 1,
                },
                text: "x".to_owned(),
            })
            .collect();
        let path = CoreProjectPath::new("lib.rs")?;
        Ok(FileRewrite::modify(path, "", String::new(), replaced))
    }

    #[test]
    fn regions_fit_the_edit_bound_is_true_at_the_cap_and_false_one_past_it() -> TestResult {
        let at_cap = vec![rewrite_with_region_count(super::CHANGE_EDITS_MAX)?];
        assert!(
            super::regions_fit_the_edit_bound(&at_cap),
            "exactly {} regions must still fit the edit bound",
            super::CHANGE_EDITS_MAX
        );
        let over_cap = vec![rewrite_with_region_count(super::CHANGE_EDITS_MAX + 1)?];
        assert!(
            !super::regions_fit_the_edit_bound(&over_cap),
            "{} regions must exceed the edit bound of {}",
            super::CHANGE_EDITS_MAX + 1,
            super::CHANGE_EDITS_MAX
        );
        Ok(())
    }

    fn warning(message: &str) -> Diagnostic {
        Diagnostic {
            severity: Severity::Warning,
            code: None,
            message: message.to_owned(),
            span: None,
            related: Vec::new(),
            tags: Vec::new(),
            reliability: DiagnosticReliability::Reliable,
            continuation: DiagnosticContinuation::Unknown,
            extensions: Extensions(std::collections::BTreeMap::new()),
            language: None,
        }
    }

    /// A deletion contributes no reparse diagnostics of its own, but the
    /// bound must still apply to it: warnings carried in from earlier in
    /// the batch - here simulated directly, since a delete no longer
    /// resolves through a symlink and so cannot generate them itself -
    /// must not outlive a batch made only of deletions.
    #[test]
    fn test_fold_and_bound_diagnostics_bounds_a_deletions_carried_over_warnings() -> TestResult {
        let mut diagnostics: Vec<Diagnostic> = (0..super::CHANGE_DIAGNOSTICS_MAX + 4)
            .map(|index| warning(&format!("carried warning {index}")))
            .collect();
        let rewrite = FileRewrite::delete(CoreProjectPath::new("gone.rs")?, "");
        let unit = FileId("rift://file/gone.rs".to_owned());
        super::fold_and_bound_diagnostics(&mut diagnostics, &rewrite, unit);
        assert_eq!(
            diagnostics.len(),
            super::CHANGE_DIAGNOSTICS_MAX,
            "a deletion contributes nothing of its own, but the bound still applies to it"
        );
        assert_eq!(diagnostics[0].message, "carried warning 0");
        assert_eq!(
            diagnostics[super::CHANGE_DIAGNOSTICS_MAX - 1].message,
            format!("carried warning {}", super::CHANGE_DIAGNOSTICS_MAX - 1)
        );
        Ok(())
    }
}
