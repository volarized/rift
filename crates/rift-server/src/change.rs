//! Change resolution and atomic application for the workspace tree.
//!
//! Every change tool resolves its address against the served index, proves
//! its preconditions against the bytes on disk, and only then writes. A
//! resolution that produces no edits is a refusal, and the tree stays
//! untouched. Application is serialized per service, so two concurrent
//! changes collide as one clean refusal rather than as interleaved bytes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use percent_encoding::percent_decode_str;
use rift_core::ProjectPath as CoreProjectPath;
use rift_core::line::{self, LineEnding};
use rift_protocol::change::{
    BODY_BYTES_MAX, BodySource, ChangeId, ChangeResult, ChangeSummary, Edit, InsertNodeParams,
    InsertPosition, InsertSymbolParams, OperationPrecondition, OperationPreconditionKind,
    OperationPreconditionStatus, PATCH_BYTES_MAX, PatchParams, PreconditionAddress,
    PreconditionValue, RefusalReason, ReplaceNodeParams, ReplaceSymbolParams,
};
use rift_protocol::read::{
    Diagnostic, DiagnosticCode, DiagnosticContinuation, DiagnosticReliability, Extensions, FileId,
    Language, Severity, SourceSpan, TextRange,
};
use rift_syntax::{ByteRange, SyntaxDocument, SyntaxSource};
use sha2::{Digest as _, Sha256};

use crate::move_file::MovePlan;
use crate::patch;
use crate::read::{
    NodeRangeResolution, ReadError, ReadFault, ReadService, digest_hex8, file_id,
    resolve_node_range,
};
use crate::remove::{RemovePlan, is_blank_content};
use crate::rename::{PlannedRewrite, RenamePlan, survivor_findings};
use crate::rewrite::{ByteFileRewrite, FileRewrite, ReplacedRegion, RewriteKind};

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

/// Visible source state captured around one hook run.
#[derive(Clone, Debug, Default)]
pub struct HookSnapshot {
    sources: BTreeMap<CoreProjectPath, HookSource>,
}

/// One visible source file's bytes and supported permissions.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HookSource {
    bytes: Option<Vec<u8>>,
    permissions: fs::Permissions,
}

impl HookSource {
    /// Captured source text when bytes fit bounds and are valid UTF-8.
    fn text(&self) -> Option<&str> {
        std::str::from_utf8(self.bytes.as_deref()?).ok()
    }
}

impl HookSnapshot {
    /// Returns paths whose source state differs between snapshots.
    #[must_use]
    pub fn changed_paths(&self, after: &Self) -> Vec<rift_protocol::read::ProjectPath> {
        source_paths(self, after)
            .filter(|path| self.sources.get(*path) != after.sources.get(*path))
            .map(|path| rift_protocol::read::ProjectPath(path.as_str().to_owned()))
            .collect()
    }

    /// Returns whether an existing source file's permissions changed.
    #[must_use]
    pub fn permissions_changed(&self, after: &Self) -> bool {
        source_paths(self, after).any(|path| {
            matches!(
                (self.sources.get(path), after.sources.get(path)),
                (Some(before), Some(after)) if before.permissions != after.permissions
            )
        })
    }

    /// Returns whether snapshots carry identical source state.
    #[must_use]
    pub fn is_unchanged(&self, after: &Self) -> bool {
        self.sources == after.sources
    }

    /// First path whose bytes crossed bounds or are not valid UTF-8.
    #[must_use]
    pub fn unavailable_path(&self) -> Option<&str> {
        self.sources
            .iter()
            .find(|(_, source)| source.text().is_none())
            .map(|(path, _)| path.as_str())
    }

    /// Requires every captured path to carry bounded UTF-8 source.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] naming first unavailable source.
    pub fn require_source_text(&self) -> Result<(), ReadError> {
        self.unavailable_path()
            .map_or(Ok(()), |path| Err(ReadFault::source_unavailable(path)))
    }
}

/// Converts bounded visible-file captures into hook source state.
fn hook_snapshot_from_files<'a>(
    files: impl IntoIterator<Item = &'a rift_index::VisibleWorkspaceEntry>,
) -> HookSnapshot {
    let sources = files
        .into_iter()
        .map(|file| {
            let path = file.path().clone();
            (
                path,
                HookSource {
                    bytes: file.bytes().map(<[u8]>::to_vec),
                    permissions: file.permissions().clone(),
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    HookSnapshot { sources }
}

/// Iterates union of two snapshot path sets in byte order.
fn source_paths<'a>(
    before: &'a HookSnapshot,
    after: &'a HookSnapshot,
) -> impl Iterator<Item = &'a CoreProjectPath> {
    let paths: BTreeSet<&CoreProjectPath> =
        before.sources.keys().chain(after.sources.keys()).collect();
    paths.into_iter()
}

/// Builds whole-file rewrites from one hook snapshot to another.
///
/// A rewrite carries source text, so a capture whose bytes crossed the workspace's
/// bounds or are not UTF-8 refuses here rather than reaching publication.
fn snapshot_rewrites(
    before: &HookSnapshot,
    after: &HookSnapshot,
) -> Result<Vec<FileRewrite>, ReadError> {
    let text = |source: &HookSource, path: &CoreProjectPath| {
        source
            .text()
            .map(str::to_owned)
            .ok_or_else(|| ReadFault::source_unavailable(path.as_str()))
    };
    source_paths(before, after)
        .filter_map(
            |path| match (before.sources.get(path), after.sources.get(path)) {
                (Some(previous), Some(next)) if previous != next => {
                    Some(text(previous, path).and_then(|previous_text| {
                        text(next, path).map(|next_text| {
                            FileRewrite::modify(path.clone(), &previous_text, next_text, Vec::new())
                                .with_permissions(next.permissions.clone())
                        })
                    }))
                }
                (Some(previous), None) => Some(
                    text(previous, path)
                        .map(|previous_text| FileRewrite::delete(path.clone(), &previous_text)),
                ),
                (None, Some(next)) => Some(text(next, path).map(|next_text| {
                    FileRewrite::create(path.clone(), next_text)
                        .with_permissions(next.permissions.clone())
                })),
                _ => None,
            },
        )
        .collect()
}

/// Builds raw-byte rewrites that restore `desired` over `current`.
fn snapshot_byte_rewrites(
    current: &HookSnapshot,
    desired: &HookSnapshot,
) -> Result<Vec<ByteFileRewrite>, ReadError> {
    source_paths(current, desired)
        .filter_map(
            |path| match (current.sources.get(path), desired.sources.get(path)) {
                (Some(previous), Some(next)) if previous != next => {
                    Some(next.bytes.as_ref().map_or_else(
                        || Err(ReadFault::source_unavailable(path.as_str())),
                        |bytes| {
                            Ok(ByteFileRewrite::modify(
                                path.clone(),
                                bytes.clone(),
                                next.permissions.clone(),
                            ))
                        },
                    ))
                }
                (Some(_), None) => Some(Ok(ByteFileRewrite::delete(path.clone()))),
                (None, Some(next)) => Some(next.bytes.as_ref().map_or_else(
                    || Err(ReadFault::source_unavailable(path.as_str())),
                    |bytes| {
                        Ok(ByteFileRewrite::create(
                            path.clone(),
                            bytes.clone(),
                            next.permissions.clone(),
                        ))
                    },
                )),
                _ => None,
            },
        )
        .collect()
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

/// What reading a [`BodySource::File`]'s file produced.
enum BodyFileRead {
    /// The file's content, within `bytes_max`.
    Fits(String),
    /// The file holds more than `bytes_max` bytes. The bounded reader stops at
    /// `bytes_max + 1`, so this counts up to that ceiling rather than the file's
    /// true length.
    Oversized(usize),
    /// The file could not be read as UTF-8 text: absent, not a plain file, denied by
    /// permissions, or holding bytes that are not valid UTF-8. `detail` is the
    /// underlying failure's own text.
    Unreadable(String),
}

/// Reads `file`'s content, stopping after `bytes_max + 1` bytes so an oversized file
/// is refused without loading the rest into memory.
fn bounded_body_file_read(file: &str, bytes_max: usize) -> BodyFileRead {
    let opened = match fs::File::open(file) {
        Ok(opened) => opened,
        Err(error) => return BodyFileRead::Unreadable(error.to_string()),
    };
    let mut buffer = Vec::new();
    let ceiling = match u64::try_from(bytes_max) {
        Ok(bytes_max) => bytes_max.saturating_add(1),
        Err(_) => u64::MAX,
    };
    if let Err(error) = opened.take(ceiling).read_to_end(&mut buffer) {
        return BodyFileRead::Unreadable(error.to_string());
    }
    if buffer.len() > bytes_max {
        return BodyFileRead::Oversized(buffer.len());
    }
    match String::from_utf8(buffer) {
        Ok(text) => BodyFileRead::Fits(text),
        Err(error) => BodyFileRead::Unreadable(error.to_string()),
    }
}

/// Refuses a [`BodySource`] whose resolved content, inline or from a file, holds more
/// than `bytes_max` bytes.
fn oversized_body_refusal(byte_count: usize, bytes_max: usize) -> ChangeResult {
    crate::rename::unsupported_refusal(format!(
        "the body holds {byte_count} bytes; at most {bytes_max} are accepted"
    ))
}

/// Refuses a [`BodySource::File`] whose file could not be read as UTF-8 text, naming
/// the file and the underlying failure.
fn body_unreadable_refusal(file: &str, detail: &str) -> ChangeResult {
    ChangeResult::Refused {
        reason: RefusalReason::UnmetPrecondition,
        preconditions: vec![OperationPrecondition::new(
            OperationPreconditionKind::BodyReadable,
            OperationPreconditionStatus::Failed,
            Vec::new(),
            Vec::new(),
            PreconditionValue::Boolean { value: true },
            PreconditionValue::Boolean { value: false },
        )],
        diagnostics: vec![crate::rename::plan_diagnostic(format!(
            "{file} could not be read for the body: {detail}"
        ))],
    }
}

/// Resolves `source` to its content: the inline text itself, or a bounded read of its
/// file. A body over `bytes_max`, inline or from a file, refuses naming the byte
/// count; a file that cannot be read as UTF-8 text refuses `unmet_precondition`
/// naming `body_readable`.
fn resolve_body_source(source: &BodySource, bytes_max: usize) -> Result<String, ChangeResult> {
    match source {
        BodySource::Inline(text) => {
            if text.len() > bytes_max {
                return Err(oversized_body_refusal(text.len(), bytes_max));
            }
            Ok(text.clone())
        }
        BodySource::File { file } => match bounded_body_file_read(file, bytes_max) {
            BodyFileRead::Fits(text) => Ok(text),
            BodyFileRead::Oversized(counted) => Err(oversized_body_refusal(counted, bytes_max)),
            BodyFileRead::Unreadable(detail) => Err(body_unreadable_refusal(file, &detail)),
        },
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

    /// Captures live visible source state for hook checks.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when visible paths, bytes, or permissions cannot be captured.
    pub fn capture_hook_snapshot(&self, reads: &ReadService) -> Result<HookSnapshot, ReadError> {
        let files = reads.capture_visible_workspace_entries()?;
        Ok(hook_snapshot_from_files(files.iter()))
    }

    /// Restores source bytes changed during one rejected hook run.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when shared publication cannot restore source state.
    pub fn restore_hook_snapshot(
        &self,
        reads: &ReadService,
        before: &HookSnapshot,
        after: &HookSnapshot,
    ) -> Result<(), ReadError> {
        let _application = self
            .application
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rewrites = snapshot_byte_rewrites(after, before)?;
        if rewrites.is_empty() {
            return Ok(());
        }
        match crate::publish::publish_byte_rewrites(reads, &self.root, &rewrites)? {
            Ok(_) => Ok(()),
            Err(refusal) => Err(ReadFault::task(
                "hook source restore",
                format!("{refusal:?}"),
            )),
        }
    }

    /// Replaces direct summary identity and edits with final hook bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when a captured path carries no source text.
    pub fn finalize_hook_result(
        &self,
        before: &HookSnapshot,
        after: &HookSnapshot,
        mut summary: ChangeSummary,
    ) -> Result<ChangeResult, ReadError> {
        let rewrites: Vec<FileRewrite> = snapshot_rewrites(before, after)?
            .into_iter()
            .filter(FileRewrite::changes_bytes)
            .collect();
        if rewrites.is_empty() {
            return Ok(ChangeResult::Unchanged);
        }
        let mut identity = Sha256::new();
        summary.paths.clear();
        summary.edits.clear();
        for rewrite in &rewrites {
            identity.update(rewrite.path.as_str().as_bytes());
            identity.update([0]);
            identity.update(rewrite.next_source.as_bytes());
            summary.paths.push(rift_protocol::read::ProjectPath(
                rewrite.path.as_str().to_owned(),
            ));
            let unit = file_id(&rewrite.path);
            summary.edits.extend(rewrite_edits(rewrite, &unit, false));
        }
        let digest = identity.finalize();
        summary.id = ChangeId(crate::read::digest_wire_hex(&digest));
        Ok(ChangeResult::Applied { summary })
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
        let body = match resolve_body_source(&params.body, BODY_BYTES_MAX) {
            Ok(body) => body,
            Err(refusal) => return Ok(refusal),
        };
        let address = parse_symbol_address(&params.symbol.0)?;
        let resolution =
            self.resolve_symbol_spans(reads, &address, |range, _source| ChangePlan {
                path: address.path.clone(),
                range,
                text: body.clone(),
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
        let body = match resolve_body_source(&params.body, BODY_BYTES_MAX) {
            Ok(body) => body,
            Err(refusal) => return Ok(refusal),
        };
        match insert_target(params)? {
            InsertTarget::BesideAnchor(anchor) => {
                self.insert_beside_anchor(reads, anchor, params.position, &body)
            }
            InsertTarget::AtFile {
                file,
                create_missing,
            } => self.insert_at_file(reads, file, params.position, &body, create_missing),
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
        let resolution = self.resolve_symbol_spans(reads, &address, |range, source| {
            let (at, text) = spliced_beside_anchor(source, range, position, &body);
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
        let body = match resolve_body_source(&params.body, BODY_BYTES_MAX) {
            Ok(body) => body,
            Err(refusal) => return Ok(refusal),
        };
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
                        text: body,
                    },
                )?;
                self.conclude(reads, resolution)
            }
        }
    }

    /// Inserts new content beside a syntax node through a witnessed address. `body` lands
    /// verbatim at the node's own boundary - no separator, no indentation carried across -
    /// since a node is not a declaration.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for a malformed address or a filesystem failure; a stale
    /// witness, or a range naming no syntax node, returns a refused [`ChangeResult`] instead.
    pub fn insert_node(
        &self,
        reads: &ReadService,
        params: &InsertNodeParams,
    ) -> Result<ChangeResult, ReadError> {
        let _application = self
            .application
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let body = match resolve_body_source(&params.body, BODY_BYTES_MAX) {
            Ok(body) => body,
            Err(refusal) => return Ok(refusal),
        };
        match resolve_node(reads, &params.anchor)? {
            NodeResolution::Refused {
                reason,
                preconditions,
            } => Ok(ChangeResult::refused(reason, preconditions)),
            NodeResolution::Verified { address, .. } => {
                let at = match params.position {
                    InsertPosition::Before => address.range.start,
                    InsertPosition::After => address.range.end,
                };
                let resolution = self.verified_against_disk(
                    reads,
                    &address.path,
                    ChangePlan {
                        path: address.path.clone(),
                        range: ByteRange { start: at, end: at },
                        text: body,
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

    /// Verifies and writes one move plan atomically.
    ///
    /// # Errors
    ///
    /// Returns `ReadError` for filesystem failure; failed conditions
    /// return refused `ChangeResult`.
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
        rewrites.push(FileRewrite::create_from(
            plan.to.clone(),
            plan.moved_next.clone(),
            plan.from.clone(),
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
        let patch = match resolve_body_source(&params.patch, PATCH_BYTES_MAX) {
            Ok(patch) => patch,
            Err(refusal) => return Ok(refusal),
        };
        let segments = patch::split_file_segments(&patch)?;
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

    /// Publishes byte-changing rewrites as one filesystem transaction, then
    /// builds result from members that changed.
    fn apply_rewrites(
        &self,
        reads: &ReadService,
        rewrites: &[FileRewrite],
    ) -> Result<ChangeResult, ReadError> {
        let mut effective: Vec<FileRewrite> = rewrites
            .iter()
            .filter(|rewrite| rewrite.changes_bytes())
            .cloned()
            .collect();
        effective.sort_by(|first, second| first.path.as_str().cmp(second.path.as_str()));
        if effective.is_empty() {
            let paths = rewrites
                .iter()
                .map(|rewrite| rewrite.path.as_str().to_owned())
                .collect();
            let source = rewrites
                .first()
                .map_or("", |rewrite| rewrite.previous_source.as_str());
            let digest = digest_hex8(source);
            return Ok(ChangeResult::refused(
                RefusalReason::UnmetPrecondition,
                vec![OperationPrecondition::new(
                    OperationPreconditionKind::SourceUnchanged,
                    OperationPreconditionStatus::Failed,
                    Vec::new(),
                    paths,
                    PreconditionValue::Text {
                        value: digest.clone(),
                    },
                    PreconditionValue::Text { value: digest },
                )],
            ));
        }
        let warnings = match crate::publish::publish_rewrites(reads, &self.root, &effective)? {
            Ok(warnings) => warnings,
            Err(refusal) => return Ok(refusal),
        };
        let ranged = regions_fit_the_edit_bound(&effective);
        let mut identity = Sha256::new();
        let mut paths = Vec::with_capacity(effective.len());
        let mut edits = Vec::with_capacity(effective.len());
        let mut diagnostics = warnings;
        for rewrite in &effective {
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
            fold_and_bound_diagnostics(reads, &mut diagnostics, rewrite, unit);
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
    /// plan through `plan`, refusing when the target is missing. `plan`
    /// also receives the indexed file's own source, for a caller that
    /// derives its inserted bytes from the text around the declaration.
    fn resolve_symbol_spans(
        &self,
        reads: &ReadService,
        address: &SymbolAddress,
        plan: impl Fn(ByteRange, &str) -> ChangePlan,
    ) -> Result<Resolution, ReadError> {
        match resolve_symbol(reads, address)? {
            SymbolResolution::Refused {
                reason,
                preconditions,
            } => Ok(Resolution::Refused {
                reason,
                preconditions,
            }),
            SymbolResolution::Declared { file, symbol } => {
                self.verified_against_disk(reads, &address.path, plan(symbol.range, file.source()))
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

/// Edits one rewrite contributes to change result: one per replaced
/// region for modification, and whole file for create or delete.
/// Batch whose regions exceed edit bound reports whole files.
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
            rewrite.previous_len(),
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
    let boundary = match position {
        InsertPosition::Before => ByteRange { start: 0, end: 0 },
        InsertPosition::After => {
            let end = existing.len() as u64;
            ByteRange { start: end, end }
        }
    };
    let (at, text) = spliced_beside_anchor(existing, boundary, position, body);
    let mut next_source = existing.to_owned();
    next_source.insert_str(
        usize::try_from(at).expect("file boundary offset fits this platform's usize"),
        &text,
    );
    (
        next_source,
        ReplacedRegion {
            range: ByteRange { start: at, end: at },
            text,
        },
    )
}

/// Returns line endings needed for one blank line between adjacent source.
fn blank_line_separator(left: &str, right: &str, ending: LineEnding) -> String {
    let ending = ending.as_str();
    let trailing_count = left
        .strip_suffix(ending)
        .map_or(0, |rest| 1 + usize::from(rest.ends_with(ending)));
    let leading_count = right
        .strip_prefix(ending)
        .map_or(0, |rest| 1 + usize::from(rest.starts_with(ending)));
    ending.repeat(2_usize.saturating_sub(trailing_count + leading_count))
}

/// Splices `body` beside declaration at `position`.
///
/// First body line inherits declaration column when bytes before declaration
/// on its line contain only spaces or tabs. Later body lines remain verbatim.
fn spliced_beside_anchor(
    source: &str,
    anchor: ByteRange,
    position: InsertPosition,
    body: &str,
) -> (u64, String) {
    let ending = source_line_ending(source);
    let start = usize::try_from(anchor.start).expect("anchor start fits this platform's usize");
    let end = usize::try_from(anchor.end).expect("anchor end fits this platform's usize");
    let line_start = source[..start]
        .rfind(char::from(line::LINE_FEED))
        .map_or(0, |index| index + 1);
    let prefix = &source[line_start..start];
    let column = if is_blank_content(prefix) { prefix } else { "" };
    match position {
        InsertPosition::Before => {
            let separator = blank_line_separator(body, &source[start..], ending);
            (anchor.start, format!("{body}{separator}{column}"))
        }
        InsertPosition::After => {
            let separator = blank_line_separator(&source[..end], body, ending);
            (anchor.end, format!("{separator}{column}{body}"))
        }
    }
}

/// The line ending `source` already uses, read from its first terminated line; `Lf` when no
/// line in `source` carries an ending at all.
fn source_line_ending(source: &str) -> LineEnding {
    line::lines_inclusive(source)
        .find_map(LineEnding::of)
        .unwrap_or(LineEnding::Lf)
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
    match resolve_node_range(file, address.range, &address.witness)? {
        NodeRangeResolution::Verified => Ok(NodeResolution::Verified { file, address }),
        NodeRangeResolution::WitnessChanged { observed } => Ok(NodeResolution::Refused {
            reason: RefusalReason::UnmetPrecondition,
            preconditions: vec![OperationPrecondition::new(
                OperationPreconditionKind::SourceUnchanged,
                OperationPreconditionStatus::Failed,
                vec![PreconditionAddress::Node { node: node.clone() }],
                vec![address.path.as_str().to_owned()],
                PreconditionValue::Text {
                    value: address.witness.clone(),
                },
                PreconditionValue::Text { value: observed },
            )],
        }),
    }
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
fn reparse_diagnostics(
    reads: &ReadService,
    unit: FileId,
    path: &CoreProjectPath,
    source: &str,
) -> Vec<Diagnostic> {
    let absolute = reads.index().root().join(path.as_str());
    let Some(provider) = reads
        .source_policy()
        .and_then(|policy| policy.language_for_path(&absolute).ok().flatten())
        .filter(|language| language.enabled())
        .and_then(rift_index::EffectiveLanguage::syntax_provider)
    else {
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
    reads: &ReadService,
    diagnostics: &mut Vec<Diagnostic>,
    rewrite: &FileRewrite,
    unit: FileId,
) {
    if !rewrite.kind.removes_file() {
        diagnostics.extend(reparse_diagnostics(
            reads,
            unit,
            &rewrite.path,
            &rewrite.next_source,
        ));
    }
    diagnostics.truncate(CHANGE_DIAGNOSTICS_MAX);
}

fn change_diagnostic(
    unit: rift_protocol::read::FileId,
    message: String,
    range: Option<ByteRange>,
    language: Language,
) -> Diagnostic {
    Diagnostic {
        severity: Severity::Error,
        code: Some(DiagnosticCode::SyntaxError.code()),
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

    use rift_core::{LanguageFileSelections, ProjectPath as CoreProjectPath, SourceVisibility};
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::change::{
        BODY_BYTES_MAX, BodySource, ChangeResult, Edit, InsertPosition, InsertSymbolParams,
        OperationPreconditionKind, PatchParams, PreconditionAddress, PreconditionValue,
        RefusalReason, ReplaceNodeParams, ReplaceSymbolParams,
    };
    use rift_protocol::configuration::{
        HistoryConfiguration, LanguageConfiguration, WorkspaceConfiguration,
    };
    use rift_protocol::read::{
        Diagnostic, DiagnosticContinuation, DiagnosticReliability, Extensions, FileId,
        GetSymbolParams, Language, NodeId, NodesParams, ProjectPath, Severity, SymbolId,
    };
    use rift_syntax::ByteRange;

    use super::ChangeService;
    use crate::read::{ReadFault, ReadService, digest_hex8};
    use crate::rewrite::{FileRewrite, REWRITE_FILE_BYTES_MAX, ReplacedRegion};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn fixture(source: &str) -> TestResult<(tempfile::TempDir, ReadService, ChangeService)> {
        fixture_with_name("lib.rs", source)
    }

    /// A one-file workspace named `name` rather than the default `lib.rs`, for a fixture
    /// whose language the extension selects.
    fn fixture_with_name(
        name: &str,
        source: &str,
    ) -> TestResult<(tempfile::TempDir, ReadService, ChangeService)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join(name), source)?;
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

    /// A read snapshot over `root` under one workspace's configured language entries.
    fn reads_with_languages(
        root: &std::path::Path,
        configuration: &WorkspaceConfiguration,
    ) -> Result<ReadService, crate::read::ReadError> {
        ReadService::build_with_languages(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            &LanguageFileSelections::from(configuration),
            HistoryConfiguration::default(),
        )
    }

    /// A read snapshot over `root` under `limits`.
    fn reads_within(
        root: &std::path::Path,
        limits: WorkspaceIndexLimits,
    ) -> Result<ReadService, crate::read::ReadError> {
        ReadService::build(
            root,
            limits,
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )
    }

    /// A workspace whose per-file capture bound is `file_bytes_max`, so a file past it
    /// is visible and carries permissions while its bytes stay uncaptured.
    fn bounded_fixture(
        files: &[(&str, &str)],
        file_bytes_max: usize,
    ) -> TestResult<(tempfile::TempDir, ReadService, ChangeService)> {
        let directory = tempfile::tempdir()?;
        for (name, source) in files {
            fs::write(directory.path().join(name), source)?;
        }
        let limits = WorkspaceIndexLimits::new(64, file_bytes_max, 1_048_576, 16, 64)?;
        let reads = reads_within(directory.path(), limits)?;
        let changes = ChangeService::new(directory.path());
        Ok((directory, reads, changes))
    }

    fn symbol(qualified_name: &str) -> SymbolId {
        language_symbol("rust", "lib.rs", qualified_name)
    }

    /// A symbol address minted the way the server itself mints one, so a test fixture never
    /// hand-guesses the wire escaping a language segment or qualified name needs.
    fn language_symbol(language_segment: &str, path: &str, qualified_name: &str) -> SymbolId {
        SymbolId(rift_core::symbol_identity(
            language_segment,
            path,
            qualified_name,
        ))
    }

    /// One language whose grammar expresses every `insert_symbol` spacing case this suite
    /// proves: two top-level declarations packed with no blank line between them, a
    /// declaration indented inside a wrapper, the same packed pair over CRLF, and two
    /// declarations sharing one physical line. Markdown, JSON, YAML, and TOML are excluded
    /// from this table: JSON's members are comma-delimited rather than blank-line separated
    /// at all, and none of the four grammars expresses a nested, indented declaration the way
    /// this suite's indentation case needs.
    struct LanguageCase {
        language_segment: &'static str,
        file_name: &'static str,
        /// Declarations `a` and `b`, packed with no blank line between them.
        packed: &'static str,
        /// `indented`'s wrapper open line, e.g. `"impl Wrapper {\n"`.
        indented_open: &'static str,
        /// `indented`'s wrapper close line, e.g. `"}\n"`.
        indented_close: &'static str,
        /// `indented`'s own indentation, repeated before every member and before an
        /// inserted declaration's first line.
        indented_column: &'static str,
        /// `packed`, with CRLF line endings.
        crlf: &'static str,
        /// Declaration `a` on its own line; `b` and `c` sharing the next line, so `c`'s
        /// prefix on that line holds `b`'s own source text rather than blank indentation.
        two_on_one_line: &'static str,
    }

    impl LanguageCase {
        /// A one-line declaration named `name`, in this case's own language.
        fn declaration(&self, name: &str) -> String {
            if self.language_segment == "rust" {
                format!("fn {name}() {{}}")
            } else {
                format!("function {name}() {{}}")
            }
        }

        /// The member declaration `Wrapper` carries for `name`, without its leading
        /// indentation or trailing line ending, e.g. `"fn a(&self) {}"` or `"a() {}"`.
        fn indented_member_declaration(&self, name: &str) -> String {
            if self.language_segment == "rust" {
                format!("fn {name}(&self) {{}}")
            } else {
                format!("{name}() {{}}")
            }
        }

        /// A wrapper containing indented declarations `a` and `b`, e.g.
        /// `"impl Wrapper {\n    fn a(&self) {}\n    fn b(&self) {}\n}\n"`.
        fn indented(&self) -> String {
            format!(
                "{open}{col}{a}\n{col}{b}\n{close}",
                open = self.indented_open,
                col = self.indented_column,
                a = self.indented_member_declaration("a"),
                b = self.indented_member_declaration("b"),
                close = self.indented_close,
            )
        }

        /// The qualification separator `Wrapper`'s member `name` uses: `::` for Rust, `.`
        /// for the ECMAScript family.
        fn indented_member(&self, name: &str) -> String {
            let separator = if self.language_segment == "rust" {
                "::"
            } else {
                "."
            };
            format!("Wrapper{separator}{name}")
        }
    }

    fn language_cases() -> Vec<LanguageCase> {
        vec![
            LanguageCase {
                language_segment: "rust",
                file_name: "lib.rs",
                packed: "fn a() {}\nfn b() {}\n",
                indented_open: "impl Wrapper {\n",
                indented_close: "}\n",
                indented_column: "    ",
                crlf: "fn a() {}\r\nfn b() {}\r\n",
                two_on_one_line: "fn a() {}\nfn b() {} fn c() {}\n",
            },
            LanguageCase {
                language_segment: "javascript",
                file_name: "index.js",
                packed: "function a() {}\nfunction b() {}\n",
                indented_open: "class Wrapper {\n",
                indented_close: "}\n",
                indented_column: "  ",
                crlf: "function a() {}\r\nfunction b() {}\r\n",
                two_on_one_line: "function a() {}\nfunction b() {} function c() {}\n",
            },
            LanguageCase {
                language_segment: "typescript",
                file_name: "index.ts",
                packed: "function a() {}\nfunction b() {}\n",
                indented_open: "class Wrapper {\n",
                indented_close: "}\n",
                indented_column: "  ",
                crlf: "function a() {}\r\nfunction b() {}\r\n",
                two_on_one_line: "function a() {}\nfunction b() {} function c() {}\n",
            },
            LanguageCase {
                language_segment: "typescript:tsx",
                file_name: "index.tsx",
                packed: "function a() {}\nfunction b() {}\n",
                indented_open: "class Wrapper {\n",
                indented_close: "}\n",
                indented_column: "  ",
                crlf: "function a() {}\r\nfunction b() {}\r\n",
                two_on_one_line: "function a() {}\nfunction b() {} function c() {}\n",
            },
        ]
    }

    #[derive(Clone, Copy)]
    struct DeclarationWriteCase {
        language_segment: &'static str,
        file_name: &'static str,
        name: &'static str,
        source: &'static str,
        declaration: &'static str,
        replacement: &'static str,
        inserted_before: &'static str,
        inserted_after: &'static str,
        replaced_source: &'static str,
        before_source: &'static str,
        after_source: &'static str,
        removed_source: &'static str,
    }

    const DECLARATION_WRITE_CASES: [DeclarationWriteCase; 8] = [
        DeclarationWriteCase {
            language_segment: "rust",
            file_name: "lib.rs",
            name: "target",
            source: "fn target() {}\nfn right() {}\n",
            declaration: "fn target() {}",
            replacement: "fn target() { let _changed = 1; }",
            inserted_before: "fn before() {}",
            inserted_after: "fn after() {}",
            replaced_source: "fn target() { let _changed = 1; }\nfn right() {}\n",
            before_source: "fn before() {}\n\nfn target() {}\nfn right() {}\n",
            after_source: "fn target() {}\n\nfn after() {}\nfn right() {}\n",
            removed_source: "fn right() {}\n",
        },
        DeclarationWriteCase {
            language_segment: "javascript",
            file_name: "index.js",
            name: "target",
            source: "function target() {}\nfunction right() {}\n",
            declaration: "function target() {}",
            replacement: "function target() { return 1; }",
            inserted_before: "function before() {}",
            inserted_after: "function after() {}",
            replaced_source: "function target() { return 1; }\nfunction right() {}\n",
            before_source: "function before() {}\n\nfunction target() {}\nfunction right() {}\n",
            after_source: "function target() {}\n\nfunction after() {}\nfunction right() {}\n",
            removed_source: "function right() {}\n",
        },
        DeclarationWriteCase {
            language_segment: "typescript",
            file_name: "index.ts",
            name: "target",
            source: "function target(): number { return 1; }\nfunction right(): void {}\n",
            declaration: "function target(): number { return 1; }",
            replacement: "function target(): number { return 2; }",
            inserted_before: "function before(): void {}",
            inserted_after: "function after(): void {}",
            replaced_source: "function target(): number { return 2; }\nfunction right(): void {}\n",
            before_source: "function before(): void {}\n\nfunction target(): number { return 1; }\nfunction right(): void {}\n",
            after_source: "function target(): number { return 1; }\n\nfunction after(): void {}\nfunction right(): void {}\n",
            removed_source: "function right(): void {}\n",
        },
        DeclarationWriteCase {
            language_segment: "typescript:tsx",
            file_name: "index.tsx",
            name: "Target",
            source: "function Target() { return <main />; }\nfunction Right() { return <aside />; }\n",
            declaration: "function Target() { return <main />; }",
            replacement: "function Target() { return <section />; }",
            inserted_before: "function Before() { return <header />; }",
            inserted_after: "function After() { return <footer />; }",
            replaced_source: "function Target() { return <section />; }\nfunction Right() { return <aside />; }\n",
            before_source: "function Before() { return <header />; }\n\nfunction Target() { return <main />; }\nfunction Right() { return <aside />; }\n",
            after_source: "function Target() { return <main />; }\n\nfunction After() { return <footer />; }\nfunction Right() { return <aside />; }\n",
            removed_source: "function Right() { return <aside />; }\n",
        },
        DeclarationWriteCase {
            language_segment: "markdown",
            file_name: "README.md",
            name: "Target",
            source: "# Target\n\ntarget body.\n\n# Right\n\nright body.\n",
            declaration: "# Target\n\ntarget body.\n\n",
            replacement: "# Target\n\nchanged body.\n\n",
            inserted_before: "# Before\n\nbefore body.",
            inserted_after: "# After\n\nafter body.\n\n",
            replaced_source: "# Target\n\nchanged body.\n\n# Right\n\nright body.\n",
            before_source: "# Before\n\nbefore body.\n\n# Target\n\ntarget body.\n\n# Right\n\nright body.\n",
            after_source: "# Target\n\ntarget body.\n\n# After\n\nafter body.\n\n# Right\n\nright body.\n",
            removed_source: "# Right\n\nright body.\n",
        },
        DeclarationWriteCase {
            language_segment: "json",
            file_name: "settings.json",
            name: "target",
            source: "{\n  \"target\": 1,\n  \"right\": 2\n}\n",
            declaration: "\"target\": 1",
            replacement: "\"target\": 3",
            inserted_before: "\"before\": 0,",
            inserted_after: ",\"after\": 0",
            replaced_source: "{\n  \"target\": 3,\n  \"right\": 2\n}\n",
            before_source: "{\n  \"before\": 0,\n\n  \"target\": 1,\n  \"right\": 2\n}\n",
            after_source: "{\n  \"target\": 1\n\n  ,\"after\": 0,\n  \"right\": 2\n}\n",
            removed_source: "{\n  \"right\": 2\n}\n",
        },
        DeclarationWriteCase {
            language_segment: "yaml",
            file_name: "settings.yaml",
            name: "target",
            source: "target: 1\nright: 2\n",
            declaration: "target: 1",
            replacement: "target: 3",
            inserted_before: "before: 0",
            inserted_after: "after: 0",
            replaced_source: "target: 3\nright: 2\n",
            before_source: "before: 0\n\ntarget: 1\nright: 2\n",
            after_source: "target: 1\n\nafter: 0\nright: 2\n",
            removed_source: "right: 2\n",
        },
        DeclarationWriteCase {
            language_segment: "toml",
            file_name: "settings.toml",
            name: "target",
            source: "target = 1\nright = 2\n",
            declaration: "target = 1",
            replacement: "target = 3",
            inserted_before: "before = 0",
            inserted_after: "after = 0",
            replaced_source: "target = 3\nright = 2\n",
            before_source: "before = 0\n\ntarget = 1\nright = 2\n",
            after_source: "target = 1\n\nafter = 0\nright = 2\n",
            removed_source: "right = 2\n",
        },
    ];

    fn declaration_write_case(language_segment: &str) -> DeclarationWriteCase {
        DECLARATION_WRITE_CASES
            .iter()
            .copied()
            .find(|case| case.language_segment == language_segment)
            .expect("declaration write case exists")
    }

    fn addressed_declaration(
        reads: &ReadService,
        case: DeclarationWriteCase,
    ) -> TestResult<SymbolId> {
        let params: GetSymbolParams = serde_json::from_value(serde_json::json!({
            "name": case.name,
            "include_body": true
        }))?;
        let hits = reads.get_symbol(&params)?.hits;
        assert_eq!(
            hits.len(),
            1,
            "{} must emit one addressed declaration",
            case.language_segment
        );
        let hit = &hits[0];
        assert_eq!(
            hit.source.as_ref().map(|source| source.text.as_str()),
            Some(case.declaration),
            "{} must emit its exact declaration bytes",
            case.language_segment
        );
        let minted = hit
            .symbol
            .id
            .clone()
            .expect("syntax declaration has an established address");
        assert_eq!(
            minted,
            language_symbol(case.language_segment, case.file_name, case.name),
            "{} must mint its language address",
            case.language_segment
        );
        Ok(minted)
    }

    async fn assert_declaration_writes(case: DeclarationWriteCase) -> TestResult {
        let mut failures = Vec::new();

        let (directory, reads, changes) = fixture_with_name(case.file_name, case.source)?;
        let target = addressed_declaration(&reads, case)?;
        applied_summary(changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: target,
                region: None,
                body: case.replacement.into(),
            },
        )?);
        let written = fs::read_to_string(directory.path().join(case.file_name))?;
        if written != case.replaced_source {
            failures.push(format!(
                "replace expected {:?}, observed {written:?}",
                case.replaced_source
            ));
        }

        let (directory, reads, changes) = fixture_with_name(case.file_name, case.source)?;
        let target = addressed_declaration(&reads, case)?;
        applied_summary(changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: Some(target),
                file: None,
                position: InsertPosition::Before,
                body: case.inserted_before.into(),
                create_missing: false,
            },
        )?);
        let written = fs::read_to_string(directory.path().join(case.file_name))?;
        if written != case.before_source {
            failures.push(format!(
                "insert before expected {:?}, observed {written:?}",
                case.before_source
            ));
        }

        let (directory, reads, changes) = fixture_with_name(case.file_name, case.source)?;
        let target = addressed_declaration(&reads, case)?;
        applied_summary(changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: Some(target),
                file: None,
                position: InsertPosition::After,
                body: case.inserted_after.into(),
                create_missing: false,
            },
        )?);
        let written = fs::read_to_string(directory.path().join(case.file_name))?;
        if written != case.after_source {
            failures.push(format!(
                "insert after expected {:?}, observed {written:?}",
                case.after_source
            ));
        }

        let (directory, reads, changes) = fixture_with_name(case.file_name, case.source)?;
        let target = addressed_declaration(&reads, case)?;
        let engines = crate::engine::EnginePool::new(
            directory.path(),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
        );
        let resolution = crate::remove::plan_remove_symbol(
            &reads,
            &engines,
            directory.path(),
            &rift_protocol::change::RemoveSymbolParams {
                symbol: target,
                force: false,
            },
        )
        .await?;
        let crate::remove::RemoveResolution::Planned(plan) = resolution else {
            panic!(
                "{} declaration removal must produce a plan",
                case.language_segment
            );
        };
        applied_summary(changes.apply_remove(&reads, &plan)?);
        engines.shutdown().await;
        let written = fs::read_to_string(directory.path().join(case.file_name))?;
        if written != case.removed_source {
            failures.push(format!(
                "remove expected {:?}, observed {written:?}",
                case.removed_source
            ));
        }

        assert!(
            failures.is_empty(),
            "{} declaration writes failed:\n{}",
            case.language_segment,
            failures.join("\n")
        );
        Ok(())
    }

    macro_rules! declaration_write_test {
        ($name:ident, $language_segment:literal) => {
            #[tokio::test]
            async fn $name() -> TestResult {
                assert_declaration_writes(declaration_write_case($language_segment)).await
            }
        };
    }

    declaration_write_test!(declaration_writes_rust, "rust");
    declaration_write_test!(declaration_writes_javascript, "javascript");
    declaration_write_test!(declaration_writes_typescript, "typescript");
    declaration_write_test!(declaration_writes_tsx, "typescript:tsx");
    declaration_write_test!(declaration_writes_markdown, "markdown");
    declaration_write_test!(declaration_writes_json, "json");
    declaration_write_test!(declaration_writes_yaml, "yaml");
    declaration_write_test!(declaration_writes_toml, "toml");

    fn applied_summary(result: ChangeResult) -> rift_protocol::change::ChangeSummary {
        match result {
            ChangeResult::Applied { summary } => summary,
            ChangeResult::Refused { reason, .. } => {
                panic!("change must land, got refusal {reason:?}")
            }
            ChangeResult::Unchanged => panic!("change must land, got unchanged result"),
        }
    }

    fn assert_source_unchanged_refusal(result: ChangeResult) {
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("a change with no byte difference must refuse, got {result:?}");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(preconditions.len(), 1);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::SourceUnchanged
        );
    }

    #[test]
    fn test_apply_rewrites_refuses_an_empty_batch_as_source_unchanged() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.apply_rewrites(&reads, &[])?;
        assert_source_unchanged_refusal(result);
        Ok(())
    }

    #[test]
    fn test_apply_rewrites_refuses_a_byte_equal_replacement_as_source_unchanged() -> TestResult {
        let source = "pub fn beacon() {}\n";
        let (directory, reads, changes) = fixture(source)?;
        let rewrites = vec![FileRewrite::modify(
            CoreProjectPath::new("lib.rs")?,
            source,
            source.to_owned(),
            vec![ReplacedRegion {
                range: ByteRange {
                    start: 0,
                    end: source.len() as u64,
                },
                text: source.to_owned(),
            }],
        )];
        let result = changes.apply_rewrites(&reads, &rewrites)?;
        assert_source_unchanged_refusal(result);
        assert_eq!(fs::read_to_string(directory.path().join("lib.rs"))?, source);
        Ok(())
    }

    #[test]
    fn test_apply_rewrites_rejects_duplicate_paths_before_publication() -> TestResult {
        let source = "pub fn beacon() {}\n";
        let (directory, reads, changes) = fixture(source)?;
        let path = CoreProjectPath::new("lib.rs")?;
        let rewrites = vec![
            FileRewrite::modify(
                path.clone(),
                source,
                "pub fn first() {}\n".to_owned(),
                Vec::new(),
            ),
            FileRewrite::modify(path, source, "pub fn second() {}\n".to_owned(), Vec::new()),
        ];

        let result = changes.apply_rewrites(&reads, &rewrites)?;
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = result
        else {
            panic!("duplicate rewrite paths must refuse before publication");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("addressed by more than one member")
        }));
        assert_eq!(fs::read_to_string(directory.path().join("lib.rs"))?, source);
        Ok(())
    }

    #[test]
    fn test_apply_rewrites_omits_unchanged_members_from_an_applied_batch() -> TestResult {
        let changed_source = "pub fn changed() {}\n";
        let unchanged_source = "pub fn steady() {}\n";
        let (directory, reads, changes) = multi_file_fixture(&[
            ("changed.rs", changed_source),
            ("steady.rs", unchanged_source),
        ])?;
        let changed_next = "pub fn renamed() {}\n";
        let rewrites = vec![
            FileRewrite::modify(
                CoreProjectPath::new("changed.rs")?,
                changed_source,
                changed_next.to_owned(),
                vec![ReplacedRegion {
                    range: ByteRange { start: 7, end: 14 },
                    text: "renamed".to_owned(),
                }],
            ),
            FileRewrite::modify(
                CoreProjectPath::new("steady.rs")?,
                unchanged_source,
                unchanged_source.to_owned(),
                vec![ReplacedRegion {
                    range: ByteRange {
                        start: 0,
                        end: unchanged_source.len() as u64,
                    },
                    text: unchanged_source.to_owned(),
                }],
            ),
        ];
        let summary = applied_summary(changes.apply_rewrites(&reads, &rewrites)?);
        assert_eq!(summary.paths, vec![ProjectPath("changed.rs".to_owned())]);
        assert_eq!(summary.edits.len(), 1);
        assert_eq!(
            fs::read_to_string(directory.path().join("changed.rs"))?,
            changed_next
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("steady.rs"))?,
            unchanged_source
        );
        Ok(())
    }

    #[test]
    fn test_apply_rewrites_orders_effective_members_by_path() -> TestResult {
        let first_source = "pub fn first() {}\n";
        let second_source = "pub fn second() {}\n";
        let (_directory, reads, changes) =
            multi_file_fixture(&[("a.rs", first_source), ("z.rs", second_source)])?;
        let rewrites = vec![
            FileRewrite::modify(
                CoreProjectPath::new("z.rs")?,
                second_source,
                "pub fn changed_second() {}\n".to_owned(),
                Vec::new(),
            ),
            FileRewrite::modify(
                CoreProjectPath::new("a.rs")?,
                first_source,
                "pub fn changed_first() {}\n".to_owned(),
                Vec::new(),
            ),
        ];

        let summary = applied_summary(changes.apply_rewrites(&reads, &rewrites)?);
        assert_eq!(
            summary.paths,
            [
                ProjectPath("a.rs".to_owned()),
                ProjectPath("z.rs".to_owned()),
            ]
        );
        Ok(())
    }

    #[test]
    fn replace_symbol_rewrites_the_declaration_atomically() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: symbol("beacon"),
                region: None,
                body: "pub fn beacon() -> u8 {\n    7\n}".to_owned().into(),
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

    /// `replace_symbol`, `insert_symbol`, and `replace_node` each accept `body` as an
    /// inline string or as an object naming a file the server reads, and both forms
    /// produce the same written bytes.
    #[test]
    fn body_carrying_tools_accept_both_inline_and_file_body_forms() -> TestResult {
        let scratch = tempfile::tempdir()?;
        let scratch_file = scratch.path().join("body.txt");

        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        fs::write(&scratch_file, "pub fn beacon() -> u8 { 1 }")?;
        applied_summary(changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: symbol("beacon"),
                region: None,
                body: BodySource::File {
                    file: scratch_file.to_string_lossy().into_owned(),
                },
            },
        )?);
        assert_eq!(
            fs::read_to_string(directory.path().join("lib.rs"))?,
            "pub fn beacon() -> u8 { 1 }\n"
        );

        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        fs::write(&scratch_file, "/// Docs.\npub fn early() {}")?;
        applied_summary(changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: Some(symbol("beacon")),
                file: None,
                position: InsertPosition::Before,
                body: BodySource::File {
                    file: scratch_file.to_string_lossy().into_owned(),
                },
                create_missing: false,
            },
        )?);
        assert_eq!(
            fs::read_to_string(directory.path().join("lib.rs"))?,
            "/// Docs.\npub fn early() {}\n\npub fn beacon() {}\n"
        );

        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let listing = reads.nodes(NodesParams {
            path: ProjectPath("lib.rs".to_owned()),
            position: 3,
            rev: None,
        })?;
        let node = listing.nodes[0].id.clone();
        fs::write(&scratch_file, "pub fn beacon() -> u8 { 2 }")?;
        applied_summary(changes.replace_node(
            &reads,
            &ReplaceNodeParams {
                node,
                region: None,
                body: BodySource::File {
                    file: scratch_file.to_string_lossy().into_owned(),
                },
            },
        )?);
        assert_eq!(
            fs::read_to_string(directory.path().join("lib.rs"))?,
            "pub fn beacon() -> u8 { 2 }"
        );
        Ok(())
    }

    /// A `file`-form body applies identically to the same content sent inline.
    #[test]
    fn replace_symbol_file_form_body_matches_the_inline_form_byte_identically() -> TestResult {
        let scratch = tempfile::tempdir()?;
        let scratch_file = scratch.path().join("body.rs");
        let body = "pub fn beacon() -> u8 {\n    9\n}";
        fs::write(&scratch_file, body)?;

        let (inline_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        applied_summary(changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: symbol("beacon"),
                region: None,
                body: body.into(),
            },
        )?);

        let (file_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        applied_summary(changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: symbol("beacon"),
                region: None,
                body: BodySource::File {
                    file: scratch_file.to_string_lossy().into_owned(),
                },
            },
        )?);

        assert_eq!(
            fs::read(inline_directory.path().join("lib.rs"))?,
            fs::read(file_directory.path().join("lib.rs"))?,
            "the inline and file forms must write byte-identical trees"
        );
        Ok(())
    }

    /// An absent file, a directory in place of the file, a file with no read
    /// permission, and a file holding bytes that are not valid UTF-8 each refuse
    /// `unmet_precondition` naming `body_readable`, and the targeted tree stays
    /// untouched.
    #[cfg(unix)]
    #[test]
    fn body_source_file_form_refuses_unreadable_files() -> TestResult {
        use std::os::unix::fs::PermissionsExt;

        let scratch = tempfile::tempdir()?;
        let denied = scratch.path().join("denied.rs");
        fs::write(&denied, "pub fn beacon() {}\n")?;
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o000))?;
        let invalid_utf8 = scratch.path().join("invalid_utf8.rs");
        fs::write(&invalid_utf8, [0xFF, 0xFE, 0xFD])?;
        let cases = [
            ("absent.rs", scratch.path().join("absent.rs")),
            ("a directory", scratch.path().to_path_buf()),
            ("no read permission", denied.clone()),
            ("bytes that are not valid UTF-8", invalid_utf8),
        ];
        for (case, file) in cases {
            let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
            let result = changes.replace_symbol(
                &reads,
                &ReplaceSymbolParams {
                    symbol: symbol("beacon"),
                    region: None,
                    body: BodySource::File {
                        file: file.to_string_lossy().into_owned(),
                    },
                },
            )?;
            let ChangeResult::Refused {
                reason,
                preconditions,
                ..
            } = result
            else {
                panic!("{case} must refuse");
            };
            assert_eq!(reason, RefusalReason::UnmetPrecondition, "{case}");
            assert_eq!(
                preconditions[0].kind,
                OperationPreconditionKind::BodyReadable,
                "{case}"
            );
            assert_eq!(
                fs::read_to_string(directory.path().join("lib.rs"))?,
                "pub fn beacon() {}\n",
                "{case} must leave the tree untouched"
            );
        }
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o644))?;
        Ok(())
    }

    /// A body at `BODY_BYTES_MAX` applies; one byte over refuses `unsupported` naming
    /// the byte count, inline and from a file alike. `insert_symbol`'s file-target
    /// create writes the body as the whole file, so the resulting file's length is the
    /// body's own length exactly - no leftover bytes from a replaced span mask the
    /// bound this test proves.
    #[test]
    fn body_source_bound_accepts_the_limit_and_refuses_one_byte_over() -> TestResult {
        let scratch = tempfile::tempdir()?;
        let at_bound = "x".repeat(BODY_BYTES_MAX);
        let over_bound = "x".repeat(BODY_BYTES_MAX + 1);

        let insert = |body: BodySource, name: &str| InsertSymbolParams {
            anchor: None,
            file: Some(ProjectPath(name.to_owned())),
            position: InsertPosition::After,
            body,
            create_missing: true,
        };

        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        applied_summary(
            changes.insert_symbol(&reads, &insert(at_bound.clone().into(), "at_bound.rs"))?,
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("at_bound.rs"))?.len(),
            BODY_BYTES_MAX
        );

        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result =
            changes.insert_symbol(&reads, &insert(over_bound.clone().into(), "over_bound.rs"))?;
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = result
        else {
            panic!("a body one byte over the bound must refuse");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert!(
            diagnostics[0]
                .message
                .contains(&(BODY_BYTES_MAX + 1).to_string())
        );

        let at_bound_file = scratch.path().join("at_bound_source.rs");
        fs::write(&at_bound_file, &at_bound)?;
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        applied_summary(changes.insert_symbol(
            &reads,
            &insert(
                BodySource::File {
                    file: at_bound_file.to_string_lossy().into_owned(),
                },
                "at_bound_from_file.rs",
            ),
        )?);
        assert_eq!(
            fs::read_to_string(directory.path().join("at_bound_from_file.rs"))?.len(),
            BODY_BYTES_MAX
        );

        let over_bound_file = scratch.path().join("over_bound_source.rs");
        fs::write(&over_bound_file, &over_bound)?;
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.insert_symbol(
            &reads,
            &insert(
                BodySource::File {
                    file: over_bound_file.to_string_lossy().into_owned(),
                },
                "over_bound_from_file.rs",
            ),
        )?;
        let ChangeResult::Refused { reason, .. } = result else {
            panic!("a file one byte over the bound must refuse");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        Ok(())
    }

    /// `replace_node` resolves its body before it resolves the addressed node, so a
    /// body one byte over `BODY_BYTES_MAX` refuses `unsupported` naming the byte
    /// count without ever reaching node resolution; the node id below is invalid on
    /// purpose to prove the refusal fires first.
    #[test]
    fn replace_node_refuses_a_body_one_byte_over_the_bound() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let over_bound = "x".repeat(BODY_BYTES_MAX + 1);
        let result = changes.replace_node(
            &reads,
            &ReplaceNodeParams {
                node: NodeId("rift://node/rust/lib.rs@0-18#aaaaaaaa".to_owned()),
                region: None,
                body: over_bound.into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = result
        else {
            panic!("a body one byte over the bound must refuse");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert!(
            diagnostics[0]
                .message
                .contains(&(BODY_BYTES_MAX + 1).to_string())
        );
        Ok(())
    }

    /// A body within its own bound can still combine with a modified file's existing
    /// content to exceed `REWRITE_FILE_BYTES_MAX`: `resolve_body_source` bounds only
    /// the body itself, so the shared rewrite-result check in `publish_rewrites` is
    /// what catches this.
    #[test]
    fn replace_symbol_refuses_when_body_and_existing_content_together_exceed_the_rewrite_bound()
    -> TestResult {
        let existing = format!(
            "pub fn beacon() {{}}\n// {}\n",
            "x".repeat(REWRITE_FILE_BYTES_MAX)
        );
        let (directory, reads, changes) = fixture(&existing)?;
        let body = "pub fn beacon() -> u8 { 1 }";
        let result = changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: symbol("beacon"),
                region: None,
                body: body.into(),
            },
        )?;
        let ChangeResult::Refused { reason, .. } = result else {
            panic!("a result past the rewrite bound must refuse even with a small body");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert_eq!(
            fs::read_to_string(directory.path().join("lib.rs"))?,
            existing,
            "a refused rewrite leaves the tree untouched"
        );
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
        let minted = hits[0]
            .symbol
            .id
            .clone()
            .expect("syntax-backed symbol is established");
        assert_eq!(minted.0, "rift://symbol/typescript:tsx/App.tsx/App");
        let result = changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: minted,
                region: None,
                body: "function App() {\n  return <main>rift</main>;\n}"
                    .to_owned()
                    .into(),
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
        let minted = hits[0]
            .symbol
            .id
            .clone()
            .expect("syntax-backed symbol is established");
        assert_eq!(
            minted.0,
            "rift://symbol/markdown/README.md/Install%20%3E%20Requirements"
        );
        let replacement = ReplaceSymbolParams {
            symbol: minted,
            region: None,
            body: "## Requirements\n\n- a newer toolchain\n".to_owned().into(),
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
        let minted = hits[0]
            .symbol
            .id
            .clone()
            .expect("syntax-backed symbol is established");
        assert_eq!(
            minted.0,
            "rift://symbol/json/settings.json/server%20%3E%20port"
        );
        let replacement = ReplaceSymbolParams {
            symbol: minted,
            region: None,
            body: "\"port\": 9090".to_owned().into(),
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
                body: "pub fn beacon() -> u8 { 7 }".to_owned().into(),
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
                        body: "pub fn beacon() -> u8 { 1 }".to_owned().into(),
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
                        body: "pub fn beacon() -> u8 { 2 }".to_owned().into(),
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
                    body: "pub fn replaced() {}".to_owned().into(),
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
                body: "pub fn beacon() -> u8 { 7 }".to_owned().into(),
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
                body: "/// Docs.\npub fn early() {}".to_owned().into(),
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
                body: "pub fn late() {}".to_owned().into(),
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
                body: "pub struct Early;".to_owned().into(),
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

    /// Insert `after` `anchor` into `source`, remove exactly the declaration that landed,
    /// and assert the file holds `source`'s own bytes again.
    async fn assert_insert_then_remove_round_trips(
        case: &LanguageCase,
        source: &'static str,
        anchor: &str,
    ) -> TestResult {
        {
            let (directory, reads, changes) = fixture_with_name(case.file_name, source)?;
            let inserted_name = "z";
            let inserted = changes.insert_symbol(
                &reads,
                &InsertSymbolParams {
                    anchor: Some(language_symbol(
                        case.language_segment,
                        case.file_name,
                        anchor,
                    )),
                    file: None,
                    position: InsertPosition::After,
                    body: case.declaration(inserted_name).into(),
                    create_missing: false,
                },
            )?;
            applied_summary(inserted);

            let reads_after_insert = ReadService::build(
                directory.path(),
                WorkspaceIndexLimits::default(),
                &SourceVisibility::default(),
                &rift_core::TextFileInclusion::default(),
                HistoryConfiguration::default(),
            )?;
            let engines = crate::engine::EnginePool::new(
                directory.path(),
                std::collections::BTreeMap::new(),
                std::collections::BTreeMap::new(),
            );
            let resolution = crate::remove::plan_remove_symbol(
                &reads_after_insert,
                &engines,
                directory.path(),
                &rift_protocol::change::RemoveSymbolParams {
                    symbol: language_symbol(case.language_segment, case.file_name, inserted_name),
                    force: false,
                },
            )
            .await?;
            let crate::remove::RemoveResolution::Planned(plan) = resolution else {
                panic!(
                    "{}: the inserted declaration must resolve for removal",
                    case.language_segment
                );
            };
            applied_summary(changes.apply_remove(&reads_after_insert, &plan)?);

            let written = fs::read_to_string(directory.path().join(case.file_name))?;
            assert_eq!(
                written, source,
                "{}: insert then remove must return the original bytes",
                case.language_segment
            );
        }
        Ok(())
    }

    /// Insert `after` a declaration, then remove exactly the declaration that landed: the
    /// file's bytes must equal the original exactly, in every language whose grammar
    /// expresses the shape. Two fixtures, because the two defects are different: a packed
    /// pair, where `widened_removal_span` used never to take back the blank line the
    /// insertion added, and a pair sharing one line, where the insertion splits that line
    /// and only the rejoin rule puts the two halves back together.
    #[tokio::test]
    async fn insert_symbol_after_then_remove_symbol_round_trips_to_the_original_bytes() -> TestResult
    {
        for case in language_cases() {
            Box::pin(assert_insert_then_remove_round_trips(
                &case,
                case.packed,
                "a",
            ))
            .await?;
            Box::pin(assert_insert_then_remove_round_trips(
                &case,
                case.two_on_one_line,
                "b",
            ))
            .await?;
        }
        Ok(())
    }

    /// Insert `before` a declaration indented inside a wrapper: the anchor keeps its
    /// original column, the inserted body's first line inherits that same column, and the
    /// body's own later lines land exactly as authored - never reindented to the anchor's
    /// column or to anything else.
    #[test]
    fn insert_symbol_before_an_indented_anchor_keeps_both_columns_and_leaves_later_body_lines_unchanged()
    -> TestResult {
        for case in language_cases() {
            let indented = case.indented();
            let (directory, reads, changes) = fixture_with_name(case.file_name, &indented)?;
            let body = format!(
                "// c\n{over_indent}// deliberately over-indented",
                over_indent = case.indented_column.repeat(3)
            );
            let result = changes.insert_symbol(
                &reads,
                &InsertSymbolParams {
                    anchor: Some(language_symbol(
                        case.language_segment,
                        case.file_name,
                        &case.indented_member("b"),
                    )),
                    file: None,
                    position: InsertPosition::Before,
                    body: body.clone().into(),
                    create_missing: false,
                },
            )?;
            applied_summary(result);
            let written = fs::read_to_string(directory.path().join(case.file_name))?;
            let expected = format!(
                "{open}{col}{a}\n{col}{body}\n\n{col}{b}\n{close}",
                open = case.indented_open,
                col = case.indented_column,
                a = case.indented_member_declaration("a"),
                b = case.indented_member_declaration("b"),
                close = case.indented_close,
            );
            assert_eq!(
                written, expected,
                "{}: the anchor and the inserted declaration must both keep the anchor's \
                 column, and the body's later line must land exactly as authored",
                case.language_segment
            );
        }
        Ok(())
    }

    #[test]
    fn replace_symbol_inside_an_indented_wrapper_replaces_attached_source_and_keeps_column()
    -> TestResult {
        for case in language_cases() {
            for (layout, column, ending) in [
                ("spaces", case.indented_column, "\n"),
                ("tabs", "\t", "\n"),
                ("crlf", case.indented_column, "\r\n"),
            ] {
                let open = case.indented_open.trim_end_matches('\n');
                let close = case.indented_close.trim_end_matches('\n');
                let documented = if case.language_segment == "rust" {
                    format!("/// Docs.{ending}{column}#[test]{ending}{column}fn b() {{}}")
                } else {
                    format!("/** Docs. */{ending}{column}b() {{}}")
                };
                let source = format!(
                    "{open}{ending}{column}{a}{ending}{column}{documented}{ending}{close}",
                    a = case.indented_member_declaration("a"),
                );
                let (directory, reads, changes) = fixture_with_name(case.file_name, &source)?;
                let body = if case.language_segment == "rust" {
                    format!(
                        "#[test]{ending}{column}fn b() {{{ending}{column}{column}// body{ending}{column}}}"
                    )
                } else {
                    format!("b() {{{ending}{column}{column}// body{ending}{column}}}")
                };
                let result = changes.replace_symbol(
                    &reads,
                    &ReplaceSymbolParams {
                        symbol: language_symbol(
                            case.language_segment,
                            case.file_name,
                            &case.indented_member("b"),
                        ),
                        region: None,
                        body: body.clone().into(),
                    },
                )?;
                applied_summary(result);
                let written = fs::read_to_string(directory.path().join(case.file_name))?;
                let expected = format!(
                    "{open}{ending}{column}{a}{ending}{column}{body}{ending}{close}",
                    a = case.indented_member_declaration("a"),
                );
                assert_eq!(
                    written, expected,
                    "{} {layout}: replacement must remove attached source, keep the declaration column, and leave later body lines unchanged",
                    case.language_segment
                );
            }
        }
        Ok(())
    }

    #[test]
    fn insert_symbol_after_an_indented_anchor_keeps_the_anchor_column() -> TestResult {
        for case in language_cases() {
            for (layout, column, ending) in [
                ("spaces", case.indented_column, "\n"),
                ("tabs", "\t", "\n"),
                ("crlf", case.indented_column, "\r\n"),
            ] {
                let open = case.indented_open.trim_end_matches('\n');
                let close = case.indented_close.trim_end_matches('\n');
                let source = format!(
                    "{open}{ending}{column}{a}{ending}{column}{b}{ending}{close}",
                    a = case.indented_member_declaration("a"),
                    b = case.indented_member_declaration("b"),
                );
                let (directory, reads, changes) = fixture_with_name(case.file_name, &source)?;
                let body = if case.language_segment == "rust" {
                    format!("fn c(&self) {{{ending}{column}{column}// body{ending}{column}}}")
                } else {
                    format!("c() {{{ending}{column}{column}// body{ending}{column}}}")
                };
                let result = changes.insert_symbol(
                    &reads,
                    &InsertSymbolParams {
                        anchor: Some(language_symbol(
                            case.language_segment,
                            case.file_name,
                            &case.indented_member("a"),
                        )),
                        file: None,
                        position: InsertPosition::After,
                        body: body.clone().into(),
                        create_missing: false,
                    },
                )?;
                applied_summary(result);
                let written = fs::read_to_string(directory.path().join(case.file_name))?;
                let expected = format!(
                    "{open}{ending}{column}{a}{ending}{ending}{column}{body}{ending}{column}{b}{ending}{close}",
                    a = case.indented_member_declaration("a"),
                    b = case.indented_member_declaration("b"),
                );
                assert_eq!(
                    written, expected,
                    "{} {layout}: inserted declaration must keep the anchor column and later body lines",
                    case.language_segment
                );
            }
        }
        Ok(())
    }

    /// Insert `after` a declaration in a CRLF file: the blank-line separator uses CRLF
    /// throughout, and no bare LF is introduced.
    #[test]
    fn insert_symbol_into_a_crlf_file_uses_crlf_for_the_separator() -> TestResult {
        for case in language_cases() {
            let (directory, reads, changes) = fixture_with_name(case.file_name, case.crlf)?;
            let inserted = case.declaration("z");
            let result = changes.insert_symbol(
                &reads,
                &InsertSymbolParams {
                    anchor: Some(language_symbol(case.language_segment, case.file_name, "a")),
                    file: None,
                    position: InsertPosition::After,
                    body: inserted.clone().into(),
                    create_missing: false,
                },
            )?;
            applied_summary(result);
            let written = fs::read_to_string(directory.path().join(case.file_name))?;
            // `crlf` is exactly `declaration("a") + "\r\n" + declaration("b") + "\r\n"`; the
            // insertion adds one CRLF-separated declaration between them.
            let expected = format!(
                "{a}\r\n\r\n{inserted}\r\n{b}\r\n",
                a = case.declaration("a"),
                b = case.declaration("b"),
            );
            assert_eq!(
                written, expected,
                "{}: the separator around the inserted declaration must use CRLF",
                case.language_segment
            );
            for line in written.split("\r\n") {
                assert!(
                    !line.contains('\n'),
                    "{}: a bare LF must never appear outside a CRLF pair: {written}",
                    case.language_segment
                );
            }
        }
        Ok(())
    }

    /// Insert `before` a declaration that shares its line with another: the shared line's
    /// text is not blank indentation, so it is never copied after the separator, and the
    /// following declaration lands at column zero rather than inheriting a false column.
    #[test]
    fn insert_symbol_before_an_anchor_sharing_its_line_never_copies_the_source_prefix() -> TestResult
    {
        for case in language_cases() {
            let (directory, reads, changes) =
                fixture_with_name(case.file_name, case.two_on_one_line)?;
            let inserted = case.declaration("x");
            let result = changes.insert_symbol(
                &reads,
                &InsertSymbolParams {
                    anchor: Some(language_symbol(case.language_segment, case.file_name, "c")),
                    file: None,
                    position: InsertPosition::Before,
                    body: inserted.clone().into(),
                    create_missing: false,
                },
            )?;
            applied_summary(result);
            let written = fs::read_to_string(directory.path().join(case.file_name))?;
            let expected = case.two_on_one_line.replacen(
                &case.declaration("c"),
                &format!("{inserted}\n\n{}", case.declaration("c")),
                1,
            );
            assert_eq!(
                written, expected,
                "{}: the shared line's own source must not be duplicated after the separator",
                case.language_segment
            );
        }
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
                body: "/// New docs.\npub struct Beacon {\n    pub signal: u8,\n}"
                    .to_owned()
                    .into(),
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
                body: "//! Module docs.".to_owned().into(),
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
                body: "pub fn tail() {}".to_owned().into(),
                create_missing: false,
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(
            written,
            "//! Module docs.\n\npub fn beacon() {}\n\npub fn tail() {}"
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
                body: "// plan".to_owned().into(),
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
                body: "pub fn late() {}".to_owned().into(),
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
                body: "pub fn late() {}".to_owned().into(),
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
                body: "pub fn late() {}".to_owned().into(),
                create_missing: true,
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(written, "pub fn beacon() {}\n\npub fn late() {}");
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
                body: "- write docs".to_owned().into(),
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
                    body: "pub fn late() {}".to_owned().into(),
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
                    body: "pub fn late() {}".to_owned().into(),
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
                    body: "pub fn late() {}".to_owned().into(),
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
                    body: "x".to_owned().into(),
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
                    body: "pub fn late() {}".to_owned().into(),
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
                body: "pub fn beacon() -> u8 { 7 }".to_owned().into(),
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
                body: "pub fn beacon() -> u8 {\n    7\n}".to_owned().into(),
            },
        )?;
        applied_summary(applied);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert!(written.contains("-> u8"));
        Ok(())
    }

    /// `insert_node` splices `body` verbatim at the node's own boundary, on either side,
    /// with no separator of its own - unlike `insert_symbol`, which always adds one.
    #[test]
    fn insert_node_lands_the_body_unchanged_with_no_separator_on_either_side() -> TestResult {
        let source = "pub fn beacon() {}\n";

        let (directory, reads, changes) = fixture(source)?;
        let node = declaration_node_id(&reads, "pub fn beacon() {}")?;
        let result = changes.insert_node(
            &reads,
            &rift_protocol::change::InsertNodeParams {
                anchor: node,
                position: InsertPosition::Before,
                body: "// note\n".to_owned().into(),
            },
        )?;
        applied_summary(result);
        assert_eq!(
            fs::read_to_string(directory.path().join("lib.rs"))?,
            "// note\npub fn beacon() {}\n",
            "a `before` insertion must land verbatim at the node's start byte, with no \
             separator"
        );

        let (directory, reads, changes) = fixture(source)?;
        let node = declaration_node_id(&reads, "pub fn beacon() {}")?;
        let result = changes.insert_node(
            &reads,
            &rift_protocol::change::InsertNodeParams {
                anchor: node,
                position: InsertPosition::After,
                body: "// tail".to_owned().into(),
            },
        )?;
        applied_summary(result);
        assert_eq!(
            fs::read_to_string(directory.path().join("lib.rs"))?,
            "pub fn beacon() {}// tail\n",
            "an `after` insertion must land verbatim at the node's end byte, with no separator"
        );
        Ok(())
    }

    /// A body one byte over `BODY_BYTES_MAX` refuses `unsupported` naming the byte count,
    /// before `insert_node` ever resolves the anchor - the same bound every other change
    /// method enforces on its own body.
    #[test]
    fn insert_node_with_an_oversized_body_refuses_unsupported() -> TestResult {
        let source = "pub fn beacon() {}\n";
        let (_directory, reads, changes) = fixture(source)?;
        let node = declaration_node_id(&reads, "pub fn beacon() {}")?;
        let over_bound = "x".repeat(BODY_BYTES_MAX + 1);
        let result = changes.insert_node(
            &reads,
            &rift_protocol::change::InsertNodeParams {
                anchor: node,
                position: InsertPosition::After,
                body: over_bound.into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = result
        else {
            panic!("a body one byte over the bound must refuse");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert!(
            diagnostics[0]
                .message
                .contains(&(BODY_BYTES_MAX + 1).to_string())
        );
        Ok(())
    }

    /// The witnessed address of the syntax node at byte 3 of `lib.rs` whose own source
    /// excerpt equals `declaration` exactly - the function item, not the enclosing
    /// `source_file` root `nodes_at` also lists at that position.
    fn declaration_node_id(reads: &ReadService, declaration: &str) -> TestResult<NodeId> {
        let listing = reads.nodes(NodesParams {
            path: ProjectPath("lib.rs".to_owned()),
            position: 3,
            rev: None,
        })?;
        let index = listing
            .source
            .iter()
            .position(|excerpt| excerpt.text == declaration)
            .ok_or("no listed node's excerpt matches the expected declaration")?;
        Ok(listing.nodes[index].id.clone())
    }

    /// `insert_node` proves its witness the same way `replace_node` does: a stale address
    /// refuses `unmet_precondition` naming `source_unchanged` rather than splicing into
    /// bytes the address no longer describes.
    #[test]
    fn insert_node_with_a_stale_witness_refuses_source_unchanged() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let address = reads
            .nodes(NodesParams {
                path: ProjectPath("lib.rs".to_owned()),
                position: 3,
                rev: None,
            })?
            .nodes[0]
            .id
            .0
            .clone();
        let mut stale = address.clone();
        stale.replace_range(stale.len() - 8.., "00000000");
        let refused = changes.insert_node(
            &reads,
            &rift_protocol::change::InsertNodeParams {
                anchor: NodeId(stale),
                position: InsertPosition::After,
                body: "pub fn late() {}".to_owned().into(),
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
        Ok(())
    }

    /// `insert_node` addressing a range that lands inside the file but names no real syntax
    /// node refuses the way `replace_node` does: an invalid request naming the range, not a
    /// witness mismatch.
    #[test]
    fn insert_node_addressing_a_range_that_is_not_a_node_refuses_the_way_replace_node_does()
    -> TestResult {
        let source = "pub fn beacon() {}\n";
        let (directory, reads, changes) = fixture(source)?;
        let start = source.find("beacon").expect("fixture names beacon");
        let end = start + "bea".len();
        let witness = digest_hex8(&source[start..end]);
        let error = changes
            .insert_node(
                &reads,
                &rift_protocol::change::InsertNodeParams {
                    anchor: NodeId(format!("rift://node/rust/lib.rs@{start}-{end}#{witness}")),
                    position: InsertPosition::After,
                    body: "x".to_owned().into(),
                },
            )
            .expect_err("a range naming no syntax node must error");
        assert_eq!(error.descriptor().code(), "invalid_request");
        assert!(
            error.to_string().contains("outside the addressed file"),
            "message must name the range, not a witness mismatch: {error}"
        );
        let untouched = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(untouched, source);
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
                body: "pub fn beacon( {".to_owned().into(),
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
                    body: "7".to_owned().into(),
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
                    body: "x".to_owned().into(),
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
                    body: "x".to_owned().into(),
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
                body: "pub fn beacon() {}".to_owned().into(),
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
                body: "pub fn late() {}".to_owned().into(),
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
                    body: "x".to_owned().into(),
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
                body: "pub fn beacon() -> u8 { 7 }".to_owned().into(),
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

    /// A change whose resulting file crosses the parser's own bound still lands - the
    /// tree is the caller's - but the result reports the crossed bound instead of
    /// leaving discovery to the next read. `BODY_BYTES_MAX` (1mb) sits well under the
    /// Rust provider's own bound (4mb), so a body this large can no longer reach
    /// `replace_symbol`'s wire surface at all; this proves `reparse_diagnostics`
    /// directly instead of steering an unreachable integration path.
    #[test]
    fn reparse_diagnostics_reports_a_source_past_the_parser_bound() -> TestResult {
        let (_directory, reads, _changes) = fixture("pub fn beacon() {}\n")?;
        let source = format!(
            "pub fn beacon() {{}}\n// {}",
            "x".repeat(rift_syntax::RustSyntaxProvider::SOURCE_BYTES_MAX_DEFAULT)
        );
        let path = CoreProjectPath::new("lib.rs").expect("fixture path is valid");
        let unit = FileId("rift://file/lib.rs".to_owned());
        let diagnostics = super::reparse_diagnostics(&reads, unit, &path, &source);
        assert_eq!(diagnostics.len(), 1);
        assert!(
            diagnostics[0]
                .message
                .contains("no longer parses within bounds"),
            "diagnostic must name the crossed bound: {}",
            diagnostics[0].message
        );
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
                    body: "7".to_owned().into(),
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
                body: "pub fn beacon() {}".to_owned().into(),
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
                    body: "x".to_owned().into(),
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
                    body: "pub fn beacon() {}".to_owned().into(),
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
                    body: "x".to_owned().into(),
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
        let (_directory, reads, _changes) = fixture("pub fn beacon() {}\n")?;
        let path = CoreProjectPath::new("notes/TODO.txt")?;
        let unit = FileId("rift://file/notes/TODO.txt".to_owned());
        let diagnostics = super::reparse_diagnostics(&reads, unit, &path, "fn broken( {");
        assert!(
            diagnostics.is_empty(),
            "an unclaimed extension has no grammar, so no findings"
        );
        Ok(())
    }

    #[test]
    fn reparse_stamps_findings_with_the_claiming_provider_language() -> TestResult {
        let (_directory, reads, _changes) = fixture("pub fn beacon() {}\n")?;
        let path = CoreProjectPath::new("lib.rs")?;
        let unit = FileId("rift://file/lib.rs".to_owned());
        let diagnostics = super::reparse_diagnostics(&reads, unit, &path, "fn broken( {");
        assert!(!diagnostics.is_empty(), "broken rust must report findings");
        assert_eq!(
            diagnostics[0].language,
            Some(Language {
                name: "rust".to_owned(),
                dialect: None,
            })
        );
        assert_eq!(
            diagnostics[0].code.as_deref(),
            Some("rift.syntax.error"),
            "provider parse findings carry one shared Rift code"
        );
        Ok(())
    }

    #[test]
    fn reparse_skips_a_disabled_language() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let mut configuration = WorkspaceConfiguration::default();
        configuration.languages.insert(
            "rust".to_owned(),
            LanguageConfiguration {
                enabled: false,
                ..LanguageConfiguration::default()
            },
        );
        let reads = reads_with_languages(directory.path(), &configuration)?;
        let path = CoreProjectPath::new("lib.rs")?;
        let unit = FileId("rift://file/lib.rs".to_owned());

        let diagnostics = super::reparse_diagnostics(&reads, unit, &path, "fn broken( {");

        assert!(
            diagnostics.is_empty(),
            "a disabled language must contribute no syntax findings"
        );
        Ok(())
    }

    #[test]
    fn replace_node_span_beyond_the_file_fails_as_invalid() -> TestResult {
        let source = "pub fn beacon() {}\n";
        let (directory, reads, changes) = fixture(source)?;
        let end = source.len() as u64 + 10;
        // A forged witness for a range wholly past the file: no legitimate listing could
        // ever carry this address, so resolution must refuse before the witness even
        // matters.
        let witness = digest_hex8("");
        let error = changes
            .replace_node(
                &reads,
                &ReplaceNodeParams {
                    node: NodeId(format!("rift://node/rust/lib.rs@0-{end}#{witness}")),
                    region: None,
                    body: "pub fn beacon() -> u8 { 7 }".to_owned().into(),
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

    #[test]
    fn replace_node_range_naming_no_syntax_node_fails_as_invalid_not_source_unchanged() -> TestResult
    {
        let source = "pub fn beacon() {}\n";
        let (directory, reads, changes) = fixture(source)?;
        // A byte range inside the file but not equal to any real node's own range: half of
        // the `beacon` identifier. Its witness is computed correctly, so a mismatch cannot
        // explain the refusal - only the missing node can.
        let start = source.find("beacon").expect("fixture names beacon");
        let end = start + "bea".len();
        let witness = digest_hex8(&source[start..end]);
        let error = changes
            .replace_node(
                &reads,
                &ReplaceNodeParams {
                    node: NodeId(format!("rift://node/rust/lib.rs@{start}-{end}#{witness}")),
                    region: None,
                    body: "x".to_owned().into(),
                },
            )
            .expect_err("a range naming no syntax node must error");
        assert_eq!(error.descriptor().code(), "invalid_request");
        assert!(
            error.to_string().contains("outside the addressed file"),
            "message must name the range, not a witness mismatch: {error}"
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

    #[cfg(unix)]
    #[test]
    fn test_hook_snapshot_detects_and_restores_permission_only_writes() -> TestResult {
        use std::os::unix::fs::PermissionsExt;

        let source = "pub fn beacon() {}\n";
        let (directory, reads, changes) = fixture(source)?;
        let target = directory.path().join("lib.rs");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640))?;
        let before = changes.capture_hook_snapshot(&reads)?;

        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
        let after = changes.capture_hook_snapshot(&reads)?;
        assert!(before.permissions_changed(&after));
        assert_eq!(
            before.changed_paths(&after),
            [ProjectPath("lib.rs".to_owned())]
        );

        changes.restore_hook_snapshot(&reads, &before, &after)?;
        assert_eq!(fs::metadata(&target)?.permissions().mode() & 0o777, 0o640);
        let restored = changes.capture_hook_snapshot(&reads)?;
        assert!(before.is_unchanged(&restored));
        assert_eq!(fs::read_to_string(target)?, source);
        Ok(())
    }

    #[test]
    fn hook_snapshot_captures_unclassified_visible_files() -> TestResult {
        let lock = "version = 4\n";
        let (directory, reads, changes) =
            multi_file_fixture(&[("lib.rs", "pub fn beacon() {}\n"), ("Cargo.lock", lock)])?;
        let before = changes.capture_hook_snapshot(&reads)?;
        fs::write(directory.path().join("Cargo.lock"), "version = 3\n")?;

        let after = changes.capture_hook_snapshot(&reads)?;

        assert_eq!(
            before.changed_paths(&after),
            [ProjectPath("Cargo.lock".to_owned())]
        );
        changes.restore_hook_snapshot(&reads, &before, &after)?;
        assert_eq!(
            fs::read_to_string(directory.path().join("Cargo.lock"))?,
            lock
        );
        Ok(())
    }

    #[test]
    fn hook_snapshot_retains_unclassified_creates_and_deletes() -> TestResult {
        let lock = "version = 4\n";
        let (directory, reads, changes) =
            multi_file_fixture(&[("lib.rs", "pub fn beacon() {}\n"), ("Cargo.lock", lock)])?;
        let published = changes.capture_hook_snapshot(&reads)?;
        fs::remove_file(directory.path().join("Cargo.lock"))?;
        fs::write(directory.path().join("justfile"), "default:\n")?;

        let live = changes.capture_hook_snapshot(&reads)?;

        assert_eq!(
            published.changed_paths(&live),
            [
                ProjectPath("Cargo.lock".to_owned()),
                ProjectPath("justfile".to_owned()),
            ]
        );
        changes.restore_hook_snapshot(&reads, &published, &live)?;
        assert_eq!(
            fs::read_to_string(directory.path().join("Cargo.lock"))?,
            lock
        );
        assert!(!directory.path().join("justfile").exists());
        Ok(())
    }

    #[test]
    fn finalize_hook_result_carries_the_files_a_hook_created_and_deleted() -> TestResult {
        let (directory, reads, changes) =
            multi_file_fixture(&[("lib.rs", "pub fn beacon() {}\n")])?;
        let params = ReplaceSymbolParams {
            symbol: symbol("beacon"),
            region: None,
            body: "pub fn beacon() -> u8 { 7 }".to_owned().into(),
        };
        let summary = applied_summary(changes.replace_symbol(&reads, &params)?);
        let before = changes.capture_hook_snapshot(&reads)?;
        let split = "pub fn split() {}\n";
        fs::write(directory.path().join("split.rs"), split)?;
        fs::remove_file(directory.path().join("lib.rs"))?;

        let after = changes.capture_hook_snapshot(&reads)?;
        let finalized = applied_summary(changes.finalize_hook_result(&before, &after, summary)?);

        assert_eq!(
            finalized.paths,
            [
                ProjectPath("lib.rs".to_owned()),
                ProjectPath("split.rs".to_owned()),
            ],
            "a hook that deletes one file and writes another reports both"
        );
        let texts: Vec<&str> = finalized
            .edits
            .iter()
            .map(|Edit::Replace { text, .. }| text.as_str())
            .collect();
        assert_eq!(
            texts,
            ["", split],
            "a deleted file's edit empties it and a created file's edit carries its whole source"
        );
        Ok(())
    }

    #[test]
    fn restoring_a_shrunk_file_the_capture_bound_left_out_refuses() -> TestResult {
        let oversized = "pub fn beacon() {}\n// padding past the capture bound\n";
        let (directory, reads, changes) = bounded_fixture(&[("lib.rs", oversized)], 16)?;
        let before = changes.capture_hook_snapshot(&reads)?;
        let shrunk = "pub fn a() {}\n";
        fs::write(directory.path().join("lib.rs"), shrunk)?;
        let after = changes.capture_hook_snapshot(&reads)?;

        let error = changes
            .restore_hook_snapshot(&reads, &before, &after)
            .expect_err("bytes the capture bound left out cannot be restored");

        let fault = error.fault();
        assert!(
            matches!(fault, ReadFault::SourceUnavailable { path } if path == "lib.rs"),
            "unexpected fault {fault:?}"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("lib.rs"))?,
            shrunk,
            "a refused restore writes nothing"
        );
        Ok(())
    }

    #[test]
    fn restoring_a_deleted_file_the_capture_bound_left_out_refuses() -> TestResult {
        let oversized = "pub fn beacon() {}\n// padding past the capture bound\n";
        let (directory, reads, changes) = bounded_fixture(&[("lib.rs", oversized)], 16)?;
        let before = changes.capture_hook_snapshot(&reads)?;
        fs::remove_file(directory.path().join("lib.rs"))?;
        let after = changes.capture_hook_snapshot(&reads)?;

        let error = changes
            .restore_hook_snapshot(&reads, &before, &after)
            .expect_err("a deleted file with no captured bytes cannot be written back");

        let fault = error.fault();
        assert!(
            matches!(fault, ReadFault::SourceUnavailable { path } if path == "lib.rs"),
            "unexpected fault {fault:?}"
        );
        assert!(
            !directory.path().join("lib.rs").exists(),
            "a refused restore creates nothing"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_apply_move_preserves_executable_permissions() -> TestResult {
        use std::os::unix::fs::PermissionsExt;

        let source = "pub fn run() {}\n";
        let (directory, reads, changes) = multi_file_fixture(&[("run.rs", source)])?;
        let from = directory.path().join("run.rs");
        fs::set_permissions(&from, fs::Permissions::from_mode(0o751))?;
        let plan = move_plan("run.rs", "moved.rs", (source, source), vec![], None);
        applied_summary(changes.apply_move(&reads, &plan)?);
        let to = directory.path().join("moved.rs");
        assert!(!from.exists());
        assert_eq!(fs::read_to_string(&to)?, source);
        assert_eq!(
            fs::metadata(&to)?.permissions().mode() & 0o777,
            0o751,
            "moving a file must retain its executable permissions"
        );
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
        let summary = applied_summary(changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?);
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
        let summary = applied_summary(changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?);
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
        let summary = applied_summary(changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?);
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
        applied_summary(changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?);
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
            body: "//! Module docs.".to_owned().into(),
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
            body: "pub fn late() {}".to_owned().into(),
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
        assert_eq!(text, "\npub fn late() {}");
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
        let (_directory, reads, _changes) = fixture("pub fn beacon() {}\n")?;
        let mut diagnostics: Vec<Diagnostic> = (0..super::CHANGE_DIAGNOSTICS_MAX + 4)
            .map(|index| warning(&format!("carried warning {index}")))
            .collect();
        let rewrite = FileRewrite::delete(CoreProjectPath::new("gone.rs")?, "");
        let unit = FileId("rift://file/gone.rs".to_owned());
        super::fold_and_bound_diagnostics(&reads, &mut diagnostics, &rewrite, unit);
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
