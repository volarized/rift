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
    ChangeId, ChangeResult, ChangeSummary, Edit, InsertPosition, OperationPrecondition,
    OperationPreconditionKind, OperationPreconditionStatus, PreconditionAddress, PreconditionValue,
    RefusalReason, ReplaceNodeParams, ReplaceSymbolParams,
};
use rift_protocol::read::{
    Diagnostic, DiagnosticContinuation, DiagnosticReliability, Extensions, Severity, SourceSpan,
    TextRange,
};
use rift_syntax::{ByteRange, RustSource, RustSyntaxLimits, RustSyntaxProvider};
use sha2::{Digest as _, Sha256};

use crate::read::{ReadError, ReadService, file_id, node_witness};

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
            return Err(ReadError::unsupported("region-scoped replacement"));
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
            return Err(ReadError::unsupported("region-scoped replacement"));
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
            return Err(ReadError::not_found(path.as_str()));
        };
        let disk = fs::read_to_string(self.root.join(path.as_str()))
            .map_err(|error| ReadError::storage(path.as_str(), "read", &error))?;
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
            return Err(ReadError::not_found(plan.path.as_str()));
        };
        let source = file.source();
        let start = usize::try_from(plan.range.start)
            .ok()
            .filter(|start| *start <= source.len() && source.is_char_boundary(*start));
        let end = usize::try_from(plan.range.end)
            .ok()
            .filter(|end| *end <= source.len() && source.is_char_boundary(*end));
        let (Some(start), Some(end)) = (start, end) else {
            return Err(ReadError::invalid("span", "outside the addressed file"));
        };
        if start > end {
            return Err(ReadError::invalid("span", "start beyond end"));
        }
        let mut next_source = String::with_capacity(source.len() - (end - start) + plan.text.len());
        next_source.push_str(&source[..start]);
        next_source.push_str(&plan.text);
        next_source.push_str(&source[end..]);

        let absolute = self.root.join(plan.path.as_str());
        let staged = absolute.with_extension("rift-staged");
        fs::write(&staged, &next_source)
            .map_err(|error| ReadError::storage(plan.path.as_str(), "stage", &error))?;
        if let Err(error) = fs::rename(&staged, &absolute) {
            let _ = fs::remove_file(&staged);
            return Err(ReadError::storage(plan.path.as_str(), "publish", &error));
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
    let malformed = || ReadError::invalid("symbol", "not a rift symbol address");
    let remainder = address
        .strip_prefix("rift://symbol/rust/")
        .ok_or_else(malformed)?;
    let (encoded_path, encoded_name) = remainder.rsplit_once('/').ok_or_else(malformed)?;
    let path = decoded(encoded_path).ok_or_else(malformed)?;
    let qualified_name = decoded(encoded_name).ok_or_else(malformed)?;
    let path = CoreProjectPath::new(path)
        .map_err(|error| ReadError::invalid("symbol", error.violation().label()))?;
    Ok((path, qualified_name))
}

/// Splits `rift://node/rust/<path>@<start>-<end>#<witness>` into its parts.
fn parse_node_address(address: &str) -> Result<NodeAddress, ReadError> {
    let malformed = || ReadError::invalid("node", "not a witnessed rift node address");
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
    let path = CoreProjectPath::new(path)
        .map_err(|error| ReadError::invalid("node", error.violation().label()))?;
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

    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::change::{
        ChangeResult, InsertPosition, InsertSymbolParams, OperationPreconditionKind,
        PreconditionValue, RefusalReason, ReplaceNodeParams, ReplaceSymbolParams,
    };
    use rift_protocol::read::{NodeId, NodesParams, ProjectPath, SymbolId};

    use super::ChangeService;
    use crate::read::ReadService;

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
}
