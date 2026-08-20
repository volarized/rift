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

use diffy::Patch;
use percent_encoding::percent_decode_str;
use rift_core::ProjectPath as CoreProjectPath;
use rift_protocol::change::{
    ChangeId, ChangeResult, ChangeSummary, Edit, InsertPosition, OperationPrecondition,
    OperationPreconditionKind, OperationPreconditionStatus, PatchParams, PreconditionAddress,
    PreconditionValue, RefusalReason, ReplaceNodeParams, ReplaceSymbolParams,
};
use rift_protocol::read::{
    Diagnostic, DiagnosticContinuation, DiagnosticReliability, Extensions, Severity, SourceSpan,
    TextRange,
};
use rift_syntax::{ByteRange, RustSource, RustSyntaxLimits, RustSyntaxProvider};
use sha2::{Digest as _, Sha256};

use crate::read::{ReadError, ReadFault, ReadService, file_id, node_witness};

/// Most re-parse findings one applied change reports.
const CHANGE_DIAGNOSTICS_MAX: usize = 16;

/// Most files one unified diff may address.
const PATCH_FILES_MAX: usize = 64;

/// Longest hunk-mismatch detail one precondition value carries.
const PATCH_MISMATCH_DETAIL_BYTES_MAX: usize = 256;

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
        let (path, qualified_name) = parse_symbol_address(&params.symbol.0)?;
        let resolution =
            self.resolve_symbol_spans(reads, &path, &qualified_name, |range| ChangePlan {
                path: path.clone(),
                range,
                text: params.body.clone(),
            })?;
        self.conclude(reads, resolution)
    }

    /// Inserts a new declaration beside the anchor symbol.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for a malformed address or a filesystem failure;
    /// a resolvable request that cannot land returns a refused
    /// [`ChangeResult`] instead.
    pub fn insert_symbol(
        &self,
        reads: &ReadService,
        params: &rift_protocol::change::InsertSymbolParams,
    ) -> Result<ChangeResult, ReadError> {
        let (path, qualified_name) = parse_symbol_address(&params.anchor.0)?;
        let body = params.body.clone();
        let position = params.position;
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
        let address = parse_node_address(&params.node.0)?;
        let Some(file) = reads.index().file(&address.path) else {
            return Ok(refused(
                RefusalReason::UnmetPrecondition,
                vec![precondition(
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
            return Ok(refused(
                RefusalReason::UnmetPrecondition,
                vec![precondition(
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
    /// # Errors
    ///
    /// Returns [`ReadError`] for a diff that does not parse, addresses an
    /// illegal path, or fails at the filesystem; hunk-context drift and
    /// file-level changes this release does not serve return a refused
    /// [`ChangeResult`] instead.
    pub fn patch(
        &self,
        reads: &ReadService,
        params: &PatchParams,
    ) -> Result<ChangeResult, ReadError> {
        let segments = split_file_segments(&params.patch)?;
        let mut rewrites: Vec<FileRewrite> = Vec::with_capacity(segments.len());
        for segment in &segments {
            let parsed = Patch::from_str(segment)
                .map_err(|error| ReadFault::invalid("patch", error.to_string()))?;
            let path = match patched_project_path(&parsed)? {
                Ok(path) => path,
                Err(refusal) => return Ok(refusal),
            };
            let Some(file) = reads.index().file(&path) else {
                return Ok(refused(
                    RefusalReason::UnmetPrecondition,
                    vec![precondition(
                        OperationPreconditionKind::TargetExists,
                        OperationPreconditionStatus::Failed,
                        Vec::new(),
                        vec![path.as_str().to_owned()],
                        PreconditionValue::Boolean { value: true },
                        PreconditionValue::Boolean { value: false },
                    )],
                ));
            };
            if let Resolution::Refused {
                reason,
                preconditions,
            } = self.verified_against_disk(
                reads,
                &path,
                ChangePlan {
                    path: path.clone(),
                    range: ByteRange { start: 0, end: 0 },
                    text: String::new(),
                },
            )? {
                return Ok(refused(reason, preconditions));
            }
            match diffy::apply(file.source(), &parsed) {
                Ok(next_source) => rewrites.push(FileRewrite {
                    path,
                    previous_len: file.source().len() as u64,
                    next_source,
                }),
                Err(error) => {
                    let mut detail = error.to_string();
                    detail.truncate(PATCH_MISMATCH_DETAIL_BYTES_MAX);
                    return Ok(refused(
                        RefusalReason::UnmetPrecondition,
                        vec![precondition(
                            OperationPreconditionKind::SourceUnchanged,
                            OperationPreconditionStatus::Failed,
                            Vec::new(),
                            vec![path.as_str().to_owned()],
                            PreconditionValue::Text {
                                value: "every hunk context matches".to_owned(),
                            },
                            PreconditionValue::Text { value: detail },
                        )],
                    ));
                }
            }
        }
        self.apply_rewrites(reads, &rewrites)
    }

    /// Stages and publishes whole-file rewrites, all or none.
    ///
    /// Every stage lands before the first rename; a failed rename restores
    /// the files already renamed from their indexed source.
    fn apply_rewrites(
        &self,
        reads: &ReadService,
        rewrites: &[FileRewrite],
    ) -> Result<ChangeResult, ReadError> {
        let guard = self
            .application
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut staged_paths = Vec::with_capacity(rewrites.len());
        for rewrite in rewrites {
            let staged = self
                .root
                .join(rewrite.path.as_str())
                .with_extension("rift-staged");
            if let Err(error) = fs::write(&staged, &rewrite.next_source) {
                for staged in &staged_paths {
                    let _ = fs::remove_file(staged);
                }
                let _ = fs::remove_file(&staged);
                return Err(ReadFault::storage(rewrite.path.as_str(), "stage", &error));
            }
            staged_paths.push(staged);
        }
        let mut published: Vec<&FileRewrite> = Vec::with_capacity(rewrites.len());
        for (rewrite, staged) in rewrites.iter().zip(&staged_paths) {
            let absolute = self.root.join(rewrite.path.as_str());
            if let Err(error) = fs::rename(staged, &absolute) {
                for landed in &published {
                    if let Some(file) = reads.index().file(&landed.path) {
                        let _ = fs::write(self.root.join(landed.path.as_str()), file.source());
                    }
                }
                for staged in &staged_paths {
                    let _ = fs::remove_file(staged);
                }
                return Err(ReadFault::storage(rewrite.path.as_str(), "publish", &error));
            }
            published.push(rewrite);
        }
        drop(guard);

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
            if let Some(file) = reads.index().file(&rewrite.path) {
                edits.push(Edit::Replace {
                    span: SourceSpan {
                        unit: file_id(file),
                        range: TextRange {
                            start: 0,
                            end: rewrite.previous_len,
                        },
                    },
                    text: rewrite.next_source.clone(),
                });
            }
            diagnostics.extend(reparse_diagnostics(
                reads,
                &rewrite.path,
                &rewrite.next_source,
            ));
            diagnostics.truncate(CHANGE_DIAGNOSTICS_MAX);
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
                symbol: rift_protocol::read::SymbolId(format!(
                    "rift://symbol/rust/{}/{}",
                    crate::read::encode_path(path.as_str()),
                    crate::read::encode_path(qualified_name),
                )),
            }]
        };
        let Some(file) = reads.index().file(path) else {
            return Ok(Resolution::Refused {
                reason: RefusalReason::UnmetPrecondition,
                preconditions: vec![precondition(
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
                preconditions: vec![precondition(
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
                preconditions: vec![precondition(
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
                preconditions: vec![precondition(
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
            } => Ok(refused(reason, preconditions)),
            Resolution::Planned(plan) => self.apply(reads, plan),
        }
    }

    /// Writes one plan atomically and reports what landed.
    ///
    /// The application lock serializes writers; the plan's source was proven
    /// against disk during resolution, and the lock holds that proof until
    /// the rename lands.
    fn apply(&self, reads: &ReadService, plan: ChangePlan) -> Result<ChangeResult, ReadError> {
        let guard = self
            .application
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        drop(guard);

        let edit = Edit::Replace {
            span: SourceSpan {
                unit: file_id(file),
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
                diagnostics: reparse_diagnostics(reads, &plan.path, &next_source),
            },
        })
    }
}

/// One whole-file rewrite a patch resolved to.
#[derive(Debug)]
struct FileRewrite {
    path: CoreProjectPath,
    previous_len: u64,
    next_source: String,
}

/// Splits one unified diff into its per-file segments. Only a header line
/// opens a segment: hunk body lines never start with `---` at column zero,
/// because context lines carry a leading space and removals a single `-`.
fn split_file_segments(patch: &str) -> Result<Vec<String>, ReadError> {
    let mut segments: Vec<String> = Vec::new();
    for line in patch.lines() {
        if line.starts_with("--- ") {
            if segments.len() == PATCH_FILES_MAX {
                return Err(ReadFault::invalid(
                    "patch",
                    format!("addresses more than {PATCH_FILES_MAX} files"),
                ));
            }
            segments.push(String::new());
        }
        if let Some(segment) = segments.last_mut() {
            segment.push_str(line);
            segment.push('\n');
        }
    }
    if segments.is_empty() {
        return Err(ReadFault::invalid("patch", "carries no `---` file header"));
    }
    Ok(segments)
}

/// Resolves the project path one parsed segment addresses, or the refusal
/// for file-level changes this release does not serve.
fn patched_project_path(
    parsed: &Patch<'_, str>,
) -> Result<Result<CoreProjectPath, ChangeResult>, ReadError> {
    let original = parsed.original().unwrap_or_default();
    let modified = parsed.modified().unwrap_or_default();
    if original == "/dev/null" || modified == "/dev/null" {
        return Ok(Err(refused(RefusalReason::Unsupported, Vec::new())));
    }
    let original = original.strip_prefix("a/").unwrap_or(original);
    let modified = modified.strip_prefix("b/").unwrap_or(modified);
    if original != modified {
        return Ok(Err(refused(RefusalReason::Unsupported, Vec::new())));
    }
    let path = CoreProjectPath::new(original).map_err(|error| {
        ReadFault::invalid("patch", rift_core::fault_label(&error.fault().violation()))
    })?;
    Ok(Ok(path))
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

/// Splits `rift://node/rust/<path>@<start>-<end>#<witness>` into its parts.
fn parse_node_address(address: &str) -> Result<NodeAddress, ReadError> {
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
    Ok(NodeAddress {
        path,
        range: ByteRange { start, end },
        witness: witness.to_owned(),
    })
}

fn decoded(encoded: &str) -> Option<String> {
    percent_decode_str(encoded)
        .decode_utf8()
        .ok()
        .map(std::borrow::Cow::into_owned)
}

fn refused(reason: RefusalReason, preconditions: Vec<OperationPrecondition>) -> ChangeResult {
    ChangeResult::Refused {
        reason,
        preconditions,
        diagnostics: Vec::new(),
    }
}

fn precondition(
    kind: OperationPreconditionKind,
    status: OperationPreconditionStatus,
    addresses: Vec<PreconditionAddress>,
    paths: Vec<String>,
    expected: PreconditionValue,
    observed: PreconditionValue,
) -> OperationPrecondition {
    OperationPrecondition {
        kind,
        status,
        addresses,
        paths: paths
            .into_iter()
            .map(rift_protocol::read::ProjectPath)
            .collect(),
        expected,
        observed,
    }
}

/// First eight hex characters of the SHA-256 of one source text.
fn digest_hex8(source: &str) -> String {
    let digest = Sha256::digest(source.as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
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
fn reparse_diagnostics(
    reads: &ReadService,
    path: &CoreProjectPath,
    source: &str,
) -> Vec<Diagnostic> {
    let Some(file) = reads.index().file(path) else {
        return Vec::new();
    };
    let provider = RustSyntaxProvider::new(RustSyntaxLimits::default());
    let parsed = provider.analyze(RustSource { path, text: source });
    match parsed {
        Err(error) => vec![change_diagnostic(
            file_id(file),
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
                    file_id(file),
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
        let reads = ReadService::build(directory.path(), WorkspaceIndexLimits::default())?;
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
                anchor: symbol("beacon"),
                position: InsertPosition::Before,
                body: "/// Docs.\npub fn early() {}".to_owned(),
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(
            written,
            "/// Docs.\npub fn early() {}\n\npub fn beacon() {}\n"
        );

        let reads = ReadService::build(directory.path(), WorkspaceIndexLimits::default())?;
        let result = changes.insert_symbol(
            &reads,
            &InsertSymbolParams {
                anchor: symbol("beacon"),
                position: InsertPosition::After,
                body: "pub fn late() {}".to_owned(),
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
    fn replace_node_verifies_its_witness_both_ways() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let listing = reads.nodes(NodesParams {
            path: ProjectPath("lib.rs".to_owned()),
            position: 3,
            projection: None,
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
    fn patch_applies_hunks_and_reports_the_rewrite() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\npub fn steady() {}\n")?;
        let patch = [
            "--- a/lib.rs",
            "+++ b/lib.rs",
            "@@ -1,2 +1,2 @@",
            "-pub fn beacon() {}",
            "+pub fn beacon() -> u8 { 7 }",
            " pub fn steady() {}",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &rift_protocol::change::PatchParams {
                patch: patch.clone(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths, vec![ProjectPath("lib.rs".to_owned())]);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(written, "pub fn beacon() -> u8 { 7 }\npub fn steady() {}\n");
        Ok(())
    }

    #[test]
    fn patch_refuses_drifted_context_and_touches_nothing() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = [
            "--- a/lib.rs",
            "+++ b/lib.rs",
            "@@ -1 +1 @@",
            "-pub fn vanished() {}",
            "+pub fn beacon() -> u8 { 7 }",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &rift_protocol::change::PatchParams {
                patch: patch.clone(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("drifted hunk context must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::SourceUnchanged
        );
        let untouched = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(untouched, "pub fn beacon() {}\n");
        Ok(())
    }

    #[test]
    fn patch_refuses_file_creation_as_unsupported() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = [
            "--- /dev/null",
            "+++ b/new.rs",
            "@@ -0,0 +1 @@",
            "+pub fn fresh() {}",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &rift_protocol::change::PatchParams {
                patch: patch.clone(),
            },
        )?;
        let ChangeResult::Refused { reason, .. } = result else {
            panic!("file creation must refuse this release");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        Ok(())
    }

    #[test]
    fn patch_rewrites_several_files_in_one_change() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(directory.path().join("aid.rs"), "pub fn aid() {}\n")?;
        let reads = ReadService::build(directory.path(), WorkspaceIndexLimits::default())?;
        let changes = ChangeService::new(directory.path());
        let patch = [
            "--- a/lib.rs",
            "+++ b/lib.rs",
            "@@ -1 +1 @@",
            "-pub fn beacon() {}",
            "+pub fn beacon() -> u8 { 7 }",
            "--- a/aid.rs",
            "+++ b/aid.rs",
            "@@ -1 +1 @@",
            "-pub fn aid() {}",
            "+pub fn aid() -> u8 { 9 }",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &rift_protocol::change::PatchParams {
                patch: patch.clone(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths.len(), 2);
        assert_eq!(summary.edits.len(), 2);
        assert!(fs::read_to_string(directory.path().join("lib.rs"))?.contains("-> u8"));
        assert!(fs::read_to_string(directory.path().join("aid.rs"))?.contains("-> u8"));
        Ok(())
    }

    #[test]
    fn patch_rejects_malformed_and_escaping_input() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let no_header = changes
            .patch(
                &reads,
                &rift_protocol::change::PatchParams {
                    patch: "not a diff".to_owned(),
                },
            )
            .expect_err("headerless input must error");
        assert_eq!(no_header.descriptor().code(), "invalid_request");
        let escaping = changes
            .patch(
                &reads,
                &rift_protocol::change::PatchParams {
                    patch: "--- a/../escape.rs\n+++ b/../escape.rs\n@@ -1 +1 @@\n-x\n+y\n"
                        .to_owned(),
                },
            )
            .expect_err("dot segments must error");
        assert_eq!(escaping.descriptor().code(), "invalid_request");
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
                anchor: symbol("vanished"),
                position: InsertPosition::After,
                body: "pub fn late() {}".to_owned(),
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
    fn patch_rejects_more_files_than_the_bound() -> TestResult {
        use std::fmt::Write as _;

        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let mut patch = String::new();
        for index in 0..=super::PATCH_FILES_MAX {
            let _ = writeln!(
                patch,
                "--- a/f{index}.rs\n+++ b/f{index}.rs\n@@ -1 +1 @@\n-x\n+y"
            );
        }
        let error = changes
            .patch(&reads, &rift_protocol::change::PatchParams { patch })
            .expect_err("a diff past the file bound must error");
        assert_eq!(error.descriptor().code(), "invalid_request");
        assert!(
            error
                .to_string()
                .contains(&format!("more than {} files", super::PATCH_FILES_MAX)),
            "message must name the bound: {error}"
        );
        Ok(())
    }

    #[test]
    fn patch_refuses_file_rename_as_unsupported() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = [
            "--- a/lib.rs",
            "+++ b/other.rs",
            "@@ -1 +1 @@",
            "-pub fn beacon() {}",
            "+pub fn beacon() -> u8 { 7 }",
            "",
        ]
        .join("\n");
        let result = changes.patch(&reads, &rift_protocol::change::PatchParams { patch })?;
        let ChangeResult::Refused { reason, .. } = result else {
            panic!("file rename must refuse this release");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        let untouched = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(untouched, "pub fn beacon() {}\n");
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
