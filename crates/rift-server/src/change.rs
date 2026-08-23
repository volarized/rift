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
    Diagnostic, DiagnosticContinuation, DiagnosticReliability, Extensions, FileId, Severity,
    SourceSpan, TextRange,
};
use rift_syntax::{ByteRange, RustSource, RustSyntaxLimits, RustSyntaxProvider};
use sha2::{Digest as _, Sha256};

use crate::patch::{self, FileRewrite, RewriteKind};
use crate::read::{ReadError, ReadFault, ReadService, digest_hex8, file_id, node_witness};

/// Most re-parse findings one applied change reports.
const CHANGE_DIAGNOSTICS_MAX: usize = 16;

/// Byte length of the hashed material a change identity keeps: 16 bytes
/// encode to the 26 base32 characters the `chg_` pattern requires.
const CHANGE_ID_BYTES: usize = 16;

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
        let (path, qualified_name) = parse_symbol_address(&params.symbol.0)?;
        let resolution =
            self.resolve_symbol_spans(reads, &path, &qualified_name, |range| ChangePlan {
                path: path.clone(),
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
        let (path, qualified_name) = parse_symbol_address(anchor)?;
        let body = body.to_owned();
        let resolution = self.resolve_symbol_spans(reads, &path, &qualified_name, |range| {
            let (at, text) = match position {
                InsertPosition::Before => (range.start, format!("{body}\n\n")),
                InsertPosition::After => (range.end, format!("\n\n{body}")),
            };
            ChangePlan {
                path: path.clone(),
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
    /// lands verbatim — no parser involvement — so any project-relative path is
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
        let absolute = self.root.join(path.as_str());
        let existing = match fs::read_to_string(&absolute) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(ReadFault::storage(path.as_str(), "read", &error)),
        };
        let rewrite = match existing {
            Some(content) => FileRewrite {
                previous_len: content.len() as u64,
                next_source: spliced_at_file_edge(&content, position, body),
                kind: RewriteKind::Modify,
                path,
            },
            None if create_missing => FileRewrite {
                path,
                kind: RewriteKind::Create,
                previous_len: 0,
                next_source: body.to_owned(),
            },
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
        let address: NodeAddress = params.node.0.parse()?;
        let Some(file) = reads.index().file(&address.path) else {
            return Ok(ChangeResult::refused(
                RefusalReason::UnmetPrecondition,
                vec![OperationPrecondition::new(
                    OperationPreconditionKind::TargetExists,
                    OperationPreconditionStatus::Failed,
                    vec![PreconditionAddress::Node {
                        node: params.node.clone(),
                    }],
                    vec![address.path.as_str().to_owned()],
                    PreconditionValue::Boolean { value: true },
                    PreconditionValue::Boolean { value: false },
                )],
            ));
        };
        let observed_witness = node_witness(file.source(), address.range);
        if observed_witness != address.witness {
            return Ok(ChangeResult::refused(
                RefusalReason::UnmetPrecondition,
                vec![OperationPrecondition::new(
                    OperationPreconditionKind::SourceUnchanged,
                    OperationPreconditionStatus::Failed,
                    vec![PreconditionAddress::Node {
                        node: params.node.clone(),
                    }],
                    vec![address.path.as_str().to_owned()],
                    PreconditionValue::Text {
                        value: address.witness,
                    },
                    PreconditionValue::Text {
                        value: observed_witness,
                    },
                )],
            ));
        }
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
        self.apply_rewrites(reads, &rewrites)
    }

    /// Restores the files a partial publish already changed: a modified or
    /// deleted file gets its indexed source back, and a created file is
    /// removed. Rollback is best-effort inside the failure path: a file
    /// whose index entry cannot serve its source stays as published, and
    /// the storage error the caller returns names the file that stopped
    /// the publish.
    fn roll_back_published(&self, reads: &ReadService, published: &[&FileRewrite]) {
        for landed in published {
            let absolute = self.root.join(landed.path.as_str());
            match landed.kind {
                RewriteKind::Create => {
                    let _ = fs::remove_file(&absolute);
                }
                RewriteKind::Modify | RewriteKind::Delete => {
                    if let Some(file) = reads.index().file(&landed.path) {
                        let _ = fs::write(&absolute, file.source());
                    }
                }
            }
        }
    }

    /// Stages and publishes whole-file rewrites, all or none.
    ///
    /// Every stage lands before the first publish; a failed publish
    /// restores every file already published, from its indexed source or,
    /// for a file this batch created, by removing it.
    fn apply_rewrites(
        &self,
        reads: &ReadService,
        rewrites: &[FileRewrite],
    ) -> Result<ChangeResult, ReadError> {
        let mut staged: Vec<Option<PathBuf>> = Vec::with_capacity(rewrites.len());
        for rewrite in rewrites {
            if rewrite.kind == RewriteKind::Delete {
                staged.push(None);
                continue;
            }
            let absolute = self.root.join(rewrite.path.as_str());
            if rewrite.kind == RewriteKind::Create
                && let Some(parent) = absolute.parent()
                && let Err(error) = fs::create_dir_all(parent)
            {
                discard_staged(staged.iter().flatten());
                return Err(ReadFault::storage(
                    rewrite.path.as_str(),
                    "create_dir",
                    &error,
                ));
            }
            let staged_path = absolute.with_extension("rift-staged");
            if let Err(error) = fs::write(&staged_path, &rewrite.next_source) {
                discard_staged(staged.iter().flatten());
                let _ = fs::remove_file(&staged_path);
                return Err(ReadFault::storage(rewrite.path.as_str(), "stage", &error));
            }
            staged.push(Some(staged_path));
        }
        let mut published: Vec<&FileRewrite> = Vec::with_capacity(rewrites.len());
        for (rewrite, staged_path) in rewrites.iter().zip(&staged) {
            let absolute = self.root.join(rewrite.path.as_str());
            let outcome = match (rewrite.kind, staged_path) {
                (RewriteKind::Delete, _) => fs::remove_file(&absolute),
                (_, Some(staged_path)) => fs::rename(staged_path, &absolute),
                (_, None) => Err(std::io::Error::other(
                    "staged rewrite is missing its staged file",
                )),
            };
            if let Err(error) = outcome {
                self.roll_back_published(reads, &published);
                discard_staged(staged.iter().flatten());
                return Err(ReadFault::storage(rewrite.path.as_str(), "publish", &error));
            }
            published.push(rewrite);
        }
        let mut identity = Sha256::new();
        let mut paths = Vec::with_capacity(rewrites.len());
        let mut edits = Vec::with_capacity(rewrites.len());
        let mut diagnostics = Vec::new();
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
            edits.push(Edit::Replace {
                span: SourceSpan {
                    unit: unit.clone(),
                    range: TextRange {
                        start: 0,
                        end: rewrite.previous_len,
                    },
                },
                text: rewrite.next_source.clone(),
            });
            if rewrite.kind != RewriteKind::Delete {
                diagnostics.extend(reparse_diagnostics(
                    unit,
                    &rewrite.path,
                    &rewrite.next_source,
                ));
                diagnostics.truncate(CHANGE_DIAGNOSTICS_MAX);
            }
        }
        let digest = identity.finalize();
        Ok(ChangeResult::Applied {
            summary: ChangeSummary {
                id: ChangeId(format!(
                    "chg_{}",
                    crate::read::digest_prefix_base32(&digest[..CHANGE_ID_BYTES])
                )),
                paths,
                edits,
                diagnostics,
                guarantees: Vec::new(),
            },
        })
    }

    /// Resolves one symbol address to its declaration span and builds the
    /// plan through `plan`, refusing when the target is missing or ambiguous.
    fn resolve_symbol_spans(
        &self,
        reads: &ReadService,
        path: &CoreProjectPath,
        qualified_name: &str,
        plan: impl Fn(ByteRange) -> ChangePlan,
    ) -> Result<Resolution, ReadError> {
        let symbol_address = || {
            vec![PreconditionAddress::Symbol {
                symbol: rift_protocol::read::SymbolId(rift_core::rust_symbol_identity(
                    path.as_str(),
                    qualified_name,
                )),
            }]
        };
        let Some(file) = reads.index().file(path) else {
            return Ok(Resolution::Refused {
                reason: RefusalReason::UnmetPrecondition,
                preconditions: vec![OperationPrecondition::new(
                    OperationPreconditionKind::TargetExists,
                    OperationPreconditionStatus::Failed,
                    symbol_address(),
                    vec![path.as_str().to_owned()],
                    PreconditionValue::Boolean { value: true },
                    PreconditionValue::Boolean { value: false },
                )],
            });
        };
        let spans: Vec<ByteRange> = file
            .syntax()
            .symbols()
            .iter()
            .filter(|symbol| symbol.qualified_name == qualified_name)
            .map(|symbol| symbol.range)
            .collect();
        match spans.as_slice() {
            [] => Ok(Resolution::Refused {
                reason: RefusalReason::UnmetPrecondition,
                preconditions: vec![OperationPrecondition::new(
                    OperationPreconditionKind::TargetExists,
                    OperationPreconditionStatus::Failed,
                    symbol_address(),
                    vec![path.as_str().to_owned()],
                    PreconditionValue::Boolean { value: true },
                    PreconditionValue::Boolean { value: false },
                )],
            }),
            [only] => self.verified_against_disk(reads, path, plan(*only)),
            several => Ok(Resolution::Refused {
                reason: RefusalReason::AmbiguousTarget,
                preconditions: vec![OperationPrecondition::new(
                    OperationPreconditionKind::TargetExists,
                    OperationPreconditionStatus::Failed,
                    symbol_address(),
                    vec![path.as_str().to_owned()],
                    PreconditionValue::Count { value: 1 },
                    PreconditionValue::Count {
                        value: several.len() as u64,
                    },
                )],
            }),
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
    /// through this rename, so the disk proof cannot race another Rift write.
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

        let absolute = self.root.join(plan.path.as_str());
        let staged = absolute.with_extension("rift-staged");
        fs::write(&staged, &next_source)
            .map_err(|error| ReadFault::storage(plan.path.as_str(), "stage", &error))?;
        if let Err(error) = fs::rename(&staged, &absolute) {
            let _ = fs::remove_file(&staged);
            return Err(ReadFault::storage(plan.path.as_str(), "publish", &error));
        }
        let unit = file_id(file.path());
        let edit = Edit::Replace {
            span: SourceSpan {
                unit: unit.clone(),
                range: TextRange {
                    start: plan.range.start,
                    end: plan.range.end,
                },
            },
            text: plan.text,
        };
        Ok(ChangeResult::Applied {
            summary: ChangeSummary {
                id: change_id(plan.path.as_str(), &next_source),
                paths: vec![rift_protocol::read::ProjectPath(
                    plan.path.as_str().to_owned(),
                )],
                edits: vec![edit],
                diagnostics: reparse_diagnostics(unit, &plan.path, &next_source),
                guarantees: Vec::new(),
            },
        })
    }
}

/// Removes every staged sidecar file, ignoring entries already gone.
fn discard_staged<'a>(staged_paths: impl IntoIterator<Item = &'a PathBuf>) {
    for staged in staged_paths {
        let _ = fs::remove_file(staged);
    }
}

/// Splices new content at a file boundary, matching the anchor-insert spacing
/// policy: one blank line separates the new content from what was already there.
fn spliced_at_file_edge(existing: &str, position: InsertPosition, body: &str) -> String {
    match position {
        InsertPosition::Before => format!("{body}\n\n{existing}"),
        InsertPosition::After => format!("{existing}\n\n{body}"),
    }
}

/// A parsed witnessed node address.
#[derive(Debug)]
struct NodeAddress {
    path: CoreProjectPath,
    range: ByteRange,
    witness: String,
}

/// Splits `rift://symbol/rust/<path>/<qualified-name>` into its decoded
/// parts.
fn parse_symbol_address(address: &str) -> Result<(CoreProjectPath, String), ReadError> {
    let malformed = || ReadFault::invalid("symbol", "not a rift symbol address");
    let remainder = address
        .strip_prefix("rift://symbol/rust/")
        .ok_or_else(malformed)?;
    let (encoded_path, encoded_name) = remainder.rsplit_once('/').ok_or_else(malformed)?;
    let path = decoded(encoded_path).ok_or_else(malformed)?;
    let qualified_name = decoded(encoded_name).ok_or_else(malformed)?;
    let path = CoreProjectPath::new(path).map_err(|error| {
        ReadFault::invalid("symbol", rift_core::fault_label(&error.fault().violation()))
    })?;
    Ok((path, qualified_name))
}

impl std::str::FromStr for NodeAddress {
    type Err = ReadError;

    /// Parses `rift://node/rust/<path>@<start>-<end>#<witness>`.
    fn from_str(address: &str) -> Result<Self, Self::Err> {
        let malformed = || ReadFault::invalid("node", "not a witnessed rift node address");
        let remainder = address
            .strip_prefix("rift://node/rust/")
            .ok_or_else(malformed)?;
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
            path,
            range: ByteRange { start, end },
            witness: witness.to_owned(),
        })
    }
}

fn decoded(encoded: &str) -> Option<String> {
    percent_decode_str(encoded)
        .decode_utf8()
        .ok()
        .map(std::borrow::Cow::into_owned)
}

/// Mints the identity of one landed change from its path and result bytes.
fn change_id(path: &str, next_source: &str) -> ChangeId {
    let digest = Sha256::digest(format!("{path}\u{0}{next_source}").as_bytes());
    ChangeId(format!(
        "chg_{}",
        crate::read::digest_prefix_base32(&digest[..CHANGE_ID_BYTES])
    ))
}

/// Re-parses the changed file and reports parser findings, bounded.
///
/// A change that breaks the syntax still lands — the tree is the caller's —
/// but the result says so instead of leaving the discovery to the next read.
/// `unit` names the changed file even when it has no prior index entry, as
/// for a file a patch just created.
fn reparse_diagnostics(unit: FileId, path: &CoreProjectPath, source: &str) -> Vec<Diagnostic> {
    let provider = RustSyntaxProvider::new(RustSyntaxLimits::default());
    let parsed = provider.analyze(RustSource { path, text: source });
    match parsed {
        Err(error) => vec![change_diagnostic(
            unit,
            format!("the changed file no longer parses within bounds: {error}"),
            None,
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
                )
            })
            .collect(),
    }
}

fn change_diagnostic(
    unit: rift_protocol::read::FileId,
    message: String,
    range: Option<ByteRange>,
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
        language: Some(rift_protocol::read::Language {
            name: "rust".to_owned(),
            dialect: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::sync::{Arc, Barrier};

    use rift_core::SourceVisibility;
    use rift_core::constants::RUST_SOURCE_BYTES_MAX_DEFAULT;
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::change::{
        ChangeResult, InsertPosition, InsertSymbolParams, OperationPreconditionKind,
        PreconditionAddress, PreconditionValue, RefusalReason, ReplaceNodeParams,
        ReplaceSymbolParams,
    };
    use rift_protocol::read::{NodeId, NodesParams, ProjectPath, SymbolId};
    use rift_syntax::ByteRange;

    use super::ChangeService;
    use crate::read::{ReadService, node_witness};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn fixture(source: &str) -> TestResult<(tempfile::TempDir, ReadService, ChangeService)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), source)?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
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
            summary.id.0.starts_with("chg_") && summary.id.0.len() == 30,
            "change id must match its pattern: {}",
            summary.id.0
        );
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(written, "pub fn beacon() -> u8 {\n    7\n}\n");
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

    #[test]
    fn replace_symbol_refuses_missing_and_ambiguous_targets() -> TestResult {
        let (_directory, reads, changes) =
            fixture("#[cfg(unix)]\npub fn beacon() {}\n#[cfg(windows)]\npub fn beacon() {}\n")?;
        let missing = changes.replace_symbol(
            &reads,
            &ReplaceSymbolParams {
                symbol: symbol("vanished"),
                region: None,
                body: "pub fn vanished() {}".to_owned(),
            },
        )?;
        let ChangeResult::Refused { preconditions, .. } = missing else {
            panic!("missing symbol must refuse");
        };
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::TargetExists
        );

        let ambiguous = changes.replace_symbol(
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
        } = ambiguous
        else {
            panic!("two declarations must refuse");
        };
        assert_eq!(reason, RefusalReason::AmbiguousTarget);
        assert_eq!(
            preconditions[0].observed,
            PreconditionValue::Count { value: 2 }
        );
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
                file: Some(ProjectPath("notes/TODO.md".to_owned())),
                position: InsertPosition::Before,
                body: "- write docs".to_owned(),
                create_missing: true,
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("notes/TODO.md"))?;
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
            "x".repeat(RUST_SOURCE_BYTES_MAX_DEFAULT)
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
        assert!(written.len() > RUST_SOURCE_BYTES_MAX_DEFAULT);
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
}
