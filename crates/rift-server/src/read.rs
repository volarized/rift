use std::collections::BTreeMap;
use std::path::Path;

use data_encoding::BASE32_NOPAD;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rift_core::ProjectPath as CoreProjectPath;
use rift_core::constants::{DIGEST_WIRE_CHARS, OPAQUE_ID_DIGEST_CHARS, RUST_READ_PROVIDER_ID};
use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, SourceVisibility};
use rift_history::{HistoryError, Repository};
use rift_index::{
    IndexedFile, SymbolMatch, WorkspaceIndex, WorkspaceIndexError, WorkspaceIndexLimits,
};
use rift_protocol::read::{
    Coverage, CoverageCompleteState, CoverageReach, CoverageScope, Digest, ExactKind, Extensions,
    FactFamily, FileId, Freshness, GetSymbolHit, GetSymbolParams, GetSymbolResult, IndexSnapshot,
    Language, Node, NodeFacet, NodeId, NodesParams, NodesResult, ProjectPath, ProviderId,
    ProviderOrigin, ReadSnapshot, RevisionId, SearchScope, SemanticCoverage, SourceExcerpt,
    SourceKind, SourceLocation, SourceUnitId, SourceUnitSpan, Symbol, SymbolFacet, SymbolId,
    SymbolOrigin, TextRange,
};
use rift_syntax::{ByteRange, RustNode, RustSymbol, RustSymbolKind, RustVisibility};
use sha2::{Digest as _, Sha256};

/// One read-service failure: what was asked, and why it cannot be served.
///
/// An indexing failure keeps the classification of the
/// [`WorkspaceIndexError`] it wraps, so a crossed parse bound surfaces as
/// `limit_exceeded` rather than as an unclassified failure.
#[derive(Debug)]
pub enum ReadFault {
    /// Workspace could not be indexed.
    Index(WorkspaceIndexError),
    /// The workspace's version control could not serve the requested revision.
    History(HistoryError),
    /// Request uses functionality this release does not serve.
    Unsupported {
        /// The unserved capability the request named.
        capability: &'static str,
    },
    /// Request is invalid for direct workspace reads.
    Invalid {
        /// The rejected request field.
        field: &'static str,
        /// The rule the field's value broke.
        violation: String,
    },
    /// Requested source does not exist.
    NotFound {
        /// The path the request addressed.
        path: String,
    },
    /// Workspace files could not be read or written.
    Storage {
        /// The path being read or written.
        path: String,
        /// The filesystem operation that failed.
        operation: &'static str,
        /// The rendered I/O failure.
        io: String,
    },
}

impl Fault for ReadFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::Index(source) => source.descriptor().name(),
            Self::History(source) => source.descriptor().name(),
            Self::Unsupported { .. } => ErrorName::Wire(ErrorCode::CapabilityUnavailable),
            Self::Invalid { .. } => ErrorName::Wire(ErrorCode::InvalidRequest),
            Self::NotFound { .. } => ErrorName::Wire(ErrorCode::ResourceNotFound),
            Self::Storage { .. } => ErrorName::Wire(ErrorCode::StorageFailure),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::Index(source) => source.context(),
            Self::History(source) => source.context(),
            Self::Unsupported { capability } => {
                vec![ErrorContext::new("capability", *capability)]
            }
            Self::Invalid { field, violation } => vec![
                ErrorContext::new("field", *field),
                ErrorContext::new("violation", violation.clone()),
            ],
            Self::NotFound { path } => vec![ErrorContext::new("path", path.clone())],
            Self::Storage {
                path,
                operation,
                io,
            } => vec![
                ErrorContext::new("path", path.clone()),
                ErrorContext::new("operation", *operation),
                ErrorContext::new("io", io.clone()),
            ],
        }
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Index(source) => Some(source),
            Self::History(source) => Some(source),
            Self::Unsupported { .. }
            | Self::Invalid { .. }
            | Self::NotFound { .. }
            | Self::Storage { .. } => None,
        }
    }
}

impl ReadFault {
    pub(crate) fn unsupported(capability: &'static str) -> ReadError {
        Error::new(Self::Unsupported { capability })
    }

    pub(crate) fn invalid(field: &'static str, violation: impl Into<String>) -> ReadError {
        Error::new(Self::Invalid {
            field,
            violation: violation.into(),
        })
    }

    pub(crate) fn not_found(path: impl Into<String>) -> ReadError {
        Error::new(Self::NotFound { path: path.into() })
    }

    pub(crate) fn storage(
        path: impl Into<String>,
        operation: &'static str,
        io: &std::io::Error,
    ) -> ReadError {
        Error::new(Self::Storage {
            path: path.into(),
            operation,
            io: io.to_string(),
        })
    }

    pub(crate) fn index(source: WorkspaceIndexError) -> ReadError {
        Error::new(Self::Index(source))
    }

    pub(crate) fn history(source: HistoryError) -> ReadError {
        Error::new(Self::History(source))
    }
}

/// Opaque read-service failure.
pub type ReadError = Error<ReadFault>;

/// Immutable direct-filesystem Rust read service.
#[derive(Debug)]
pub struct ReadService {
    index: WorkspaceIndex,
    snapshot: ReadSnapshot,
    /// The resolved commit this service serves, or null for the current tree.
    revision: Option<RevisionId>,
}

impl ReadService {
    /// Builds one in-memory snapshot from real workspace files, applying
    /// `visibility`'s `.gitignore` and `[source]` policy on top of the hard
    /// floor.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when root cannot be indexed within bounds.
    pub fn build(
        root: &Path,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
    ) -> Result<Self, ReadError> {
        let index = WorkspaceIndex::build(root, limits, visibility).map_err(ReadFault::index)?;
        let snapshot = captured_snapshot(&index, None);
        Ok(Self {
            index,
            snapshot,
            revision: None,
        })
    }

    /// Builds one in-memory snapshot of the workspace at a version-control
    /// revision, read in place from the workspace's repository with no
    /// checkout. The revision tree passes the same `[source]` policy and
    /// bounds as the workspace scan.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when the spelling breaks the advertised
    /// charset, the workspace has no repository, the revision does not
    /// resolve to a commit, or the revision tree cannot be indexed within
    /// bounds.
    pub fn at_revision(
        root: &Path,
        rev: &RevisionId,
        limits: WorkspaceIndexLimits,
        visibility: &SourceVisibility,
    ) -> Result<Self, ReadError> {
        if let Some(violation) = rev.violation() {
            return Err(ReadFault::invalid("rev", violation.as_str()));
        }
        let repository = Repository::open(root).map_err(ReadFault::history)?;
        let resolved = repository.resolve(&rev.0).map_err(ReadFault::history)?;
        let index = WorkspaceIndex::at_revision(&repository, &resolved, limits, visibility)
            .map_err(ReadFault::index)?;
        let revision = RevisionId(resolved.commit_id());
        let snapshot = captured_snapshot(&index, Some(revision.clone()));
        Ok(Self {
            index,
            snapshot,
            revision: Some(revision),
        })
    }

    /// Returns the immutable workspace index this snapshot serves.
    pub(crate) const fn index(&self) -> &WorkspaceIndex {
        &self.index
    }

    /// Returns the tree and index revisions captured for this snapshot.
    pub(crate) const fn snapshot(&self) -> &ReadSnapshot {
        &self.snapshot
    }

    /// Returns the resolved commit this service serves, or none for the
    /// current tree.
    pub(crate) const fn revision(&self) -> Option<&RevisionId> {
        self.revision.as_ref()
    }

    /// Reads Rust syntax nodes covering one UTF-8 byte position. The tree
    /// the nodes come from is the one this service holds; `params.rev` was
    /// already honored by building the service at that revision.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for projections, invalid paths, or missing files.
    pub fn nodes(&self, params: NodesParams) -> Result<NodesResult, ReadError> {
        validate_common(
            false,
            params.projection.is_some(),
            params.rev.is_some(),
            SearchScope::Project,
        )?;
        let path = CoreProjectPath::new(params.path.0).map_err(|error| {
            ReadFault::invalid("path", rift_core::fault_label(&error.fault().violation()))
        })?;
        let file = self
            .index
            .file(&path)
            .ok_or_else(|| ReadFault::not_found(path.as_str()))?;
        let nodes = self
            .index
            .nodes(&path, params.position)
            .ok_or_else(|| ReadFault::not_found(path.as_str()))?
            .into_iter()
            .map(|node| wire_node(file, node))
            .collect();
        let source = vec![excerpt(
            file,
            ByteRange {
                start: params.position,
                end: params.position,
            },
        )];
        Ok(NodesResult {
            nodes,
            source,
            coverage: semantic_coverage(FactFamily::Nodes, &self.snapshot),
            snapshot: self.snapshot.clone(),
        })
    }

    /// Finds Rust declarations by name.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for unsupported history, cursor, projection, or scope.
    pub fn get_symbol(&self, params: &GetSymbolParams) -> Result<GetSymbolResult, ReadError> {
        validate_common(
            params.cursor.is_some(),
            params.projection.is_some(),
            params.rev.is_some(),
            params.scope,
        )?;
        if params.include_history {
            return Err(ReadFault::unsupported("symbol history"));
        }
        let limit = admitted_limit(params.limit)?;
        let hits = self
            .index
            .symbols(&params.name, limit)
            .map_err(ReadFault::index)?
            .into_iter()
            .map(|matched| GetSymbolHit {
                symbol: wire_symbol(matched),
                node: params.include_body.then(|| symbol_node(matched)),
                source: params
                    .include_body
                    .then(|| excerpt(matched.file, matched.symbol.range)),
                history: None,
                co_changes: None,
            })
            .collect();
        Ok(GetSymbolResult {
            hits,
            coverage: complete_coverage(&self.snapshot),
            next_cursor: None,
            snapshot: self.snapshot.clone(),
        })
    }
}

/// Admits a caller-supplied result limit: positive, and inside this
/// platform's addressable range.
pub(crate) fn admitted_limit(requested: u64) -> Result<usize, ReadError> {
    if requested == 0 {
        return Err(ReadFault::invalid("limit", "zero"));
    }
    usize::try_from(requested)
        .map_err(|_| ReadFault::invalid("limit", format!("{requested} exceeds this platform")))
}

pub(crate) fn validate_common(
    cursor: bool,
    projection: bool,
    rev: bool,
    scope: SearchScope,
) -> Result<(), ReadError> {
    if rev && projection {
        return Err(ReadFault::invalid(
            "rev",
            "combines with projection; a read serves one tree",
        ));
    }
    if cursor || projection {
        return Err(ReadFault::unsupported("cursor and projection reads"));
    }
    if scope == SearchScope::Dependencies {
        return Err(ReadFault::unsupported("dependency reads"));
    }
    Ok(())
}

fn wire_node(file: &IndexedFile, node: &RustNode) -> Node {
    Node {
        id: node_id(file, node),
        symbol: symbol_for_range(file, node.range).map(|symbol| symbol_id(file, symbol)),
        unit: file_id(file),
        language: rust_language(),
        kind: ExactKind(format!("rust.{}", node.kind)),
        facets: node_facets(node),
        range: text_range(node.range),
        regions: Vec::new(),
        parent: None,
        extensions: Extensions(BTreeMap::new()),
    }
}

fn symbol_node(matched: SymbolMatch<'_>) -> Node {
    let node = matched
        .file
        .syntax()
        .nodes()
        .iter()
        .find(|node| node.range == matched.symbol.range);
    node.map_or_else(
        || Node {
            id: NodeId(node_address(matched.file, matched.symbol.range)),
            symbol: Some(symbol_id(matched.file, matched.symbol)),
            unit: file_id(matched.file),
            language: rust_language(),
            kind: ExactKind(symbol_kind(matched.symbol.kind).to_owned()),
            facets: vec![NodeFacet::Declaration, NodeFacet::Definition],
            range: text_range(matched.symbol.range),
            regions: Vec::new(),
            parent: None,
            extensions: Extensions(BTreeMap::new()),
        },
        |node| wire_node(matched.file, node),
    )
}

pub(crate) fn wire_symbol(matched: SymbolMatch<'_>) -> Symbol {
    let symbol = matched.symbol;
    Symbol {
        id: symbol_id(matched.file, symbol),
        language: rust_language(),
        name: symbol.name.clone(),
        kind: ExactKind(symbol_kind(symbol.kind).to_owned()),
        facets: symbol_facets(symbol.kind, &symbol.visibility),
        origin: SymbolOrigin {
            location: Some(SourceLocation::Project { package: None }),
            source_kind: SourceKind::Authored,
            unit: Some(source_unit_id(matched.file)),
        },
        container: symbol
            .qualified_name
            .rsplit_once("::")
            .map(|(container, _)| {
                SymbolId(format!(
                    "rift://symbol/rust/{}/{}",
                    encode_path(matched.file.path().as_str()),
                    encode_path(container)
                ))
            }),
        modifiers: Vec::new(),
        visibility: Some(authored_visibility(&symbol.visibility)),
        types: Vec::new(),
        signatures: Vec::new(),
        documentation: Vec::new(),
        extensions: Extensions(BTreeMap::new()),
        document_local: false,
    }
}

pub(crate) fn excerpt(file: &IndexedFile, range: ByteRange) -> SourceExcerpt {
    let start = usize::try_from(range.start)
        .unwrap_or(file.source().len())
        .min(file.source().len());
    let end = usize::try_from(range.end)
        .unwrap_or(file.source().len())
        .min(file.source().len());
    let text = file.source().get(start..end).unwrap_or_default().to_owned();
    SourceExcerpt {
        span: source_span(file, range),
        text,
    }
}

pub(crate) fn source_span(file: &IndexedFile, range: ByteRange) -> SourceUnitSpan {
    SourceUnitSpan {
        unit: source_unit_id(file),
        range: text_range(range),
    }
}

fn text_range(range: ByteRange) -> TextRange {
    TextRange {
        start: range.start,
        end: range.end,
    }
}

pub(crate) fn rust_language() -> Language {
    Language {
        name: "rust".to_owned(),
        dialect: None,
    }
}

pub(crate) fn file_id(file: &IndexedFile) -> FileId {
    FileId(format!("rift://file/{}", encode_path(file.path().as_str())))
}

/// Mints the project resolver's source-unit identity: the resolver name, then the
/// project-relative path as the resolver's own canonical unit key.
pub(crate) fn source_unit_id(file: &IndexedFile) -> SourceUnitId {
    SourceUnitId(format!(
        "rift://source/project/{}",
        encode_path(file.path().as_str())
    ))
}

/// Project-relative path of one indexed file, as the wire model carries it.
pub(crate) fn project_path(file: &IndexedFile) -> ProjectPath {
    ProjectPath(file.path().as_str().to_owned())
}

fn symbol_id(file: &IndexedFile, symbol: &RustSymbol) -> SymbolId {
    SymbolId(format!(
        "rift://symbol/rust/{}/{}",
        encode_path(file.path().as_str()),
        encode_path(&symbol.qualified_name)
    ))
}

fn node_id(file: &IndexedFile, node: &RustNode) -> NodeId {
    NodeId(node_address(file, node.range))
}

fn node_address(file: &IndexedFile, range: ByteRange) -> String {
    format!(
        "rift://node/rust/{}@{}-{}#{}",
        encode_path(file.path().as_str()),
        range.start,
        range.end,
        node_witness(file.source(), range)
    )
}

/// The snapshot one read service captures at build time: every revision
/// field is the indexed tree's content digest, and `revision` carries the
/// resolved commit for a revision-addressed service.
fn captured_snapshot(index: &WorkspaceIndex, revision: Option<RevisionId>) -> ReadSnapshot {
    let digest = wire_digest(&workspace_digest(index));
    ReadSnapshot {
        tree_revision: digest.clone(),
        index: Some(IndexSnapshot {
            revision: digest.clone(),
            tree_revision: digest.clone(),
            freshness: Freshness::Current,
            source_revision: digest.clone(),
        }),
        source_revision: digest,
        revision,
    }
}

fn workspace_digest(index: &WorkspaceIndex) -> String {
    let mut hasher = Sha256::new();
    for file in index.files() {
        hasher.update(file.path().as_str().as_bytes());
        hasher.update([0]);
        hasher.update(file.source().as_bytes());
        hasher.update([0xff]);
    }
    format!("{:x}", hasher.finalize())
}

/// The witness a node address carries: the first eight lowercase hex characters of the
/// SHA-256 of the node's source bytes. Recomputing it is how resolution proves the bytes
/// behind an address have not drifted.
pub(crate) fn node_witness(source: &str, range: ByteRange) -> String {
    let start = usize::try_from(range.start)
        .unwrap_or(source.len())
        .min(source.len());
    let end = usize::try_from(range.end)
        .unwrap_or(source.len())
        .min(source.len());
    digest_hex8(source.get(start..end).unwrap_or_default())
}

/// First `DIGEST_WIRE_CHARS` lowercase hex characters of the SHA-256 of `source` — the sole
/// wire constructor for a witness or a `Digest`. A 64-character digest reaching the wire is a
/// defect this stays the single choke point against.
pub(crate) fn digest_hex8(source: &str) -> String {
    let fingerprint = Sha256::digest(source.as_bytes());
    format!("{fingerprint:x}")[..DIGEST_WIRE_CHARS].to_owned()
}

/// Truncates an already-hashed full-length hex digest to its wire form. `full` keeps
/// collision resistance for internal identity computation; only the truncated form crosses
/// the wire boundary.
fn wire_digest(full: &str) -> Digest {
    Digest(full[..DIGEST_WIRE_CHARS].to_owned())
}

/// Renders the leading `OPAQUE_ID_DIGEST_CHARS` base32 characters of one digest, minting an
/// opaque `chg_`-style identity.
///
/// RFC 4648 base32 omits `0`, `1`, `8`, and `9` to avoid confusion with `O`, `I`, `B`, and
/// `g`; opaque identities use its lowercase form.
pub(crate) fn digest_prefix_base32(bytes: &[u8]) -> String {
    let mut encoded = BASE32_NOPAD.encode(bytes).to_ascii_lowercase();
    encoded.truncate(OPAQUE_ID_DIGEST_CHARS);
    encoded
}

/// ASCII bytes percent-encoded inside `rift://` path segments.
///
/// The kept characters are the RFC 3986 path set; every other byte,
/// including each byte of multi-byte UTF-8 sequences, is `%XX`-escaped.
const PATH_ESCAPE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=')
    .remove(b':')
    .remove(b'@')
    .remove(b'/')
    .remove(b'-');

pub(crate) fn encode_path(value: &str) -> String {
    utf8_percent_encode(value, PATH_ESCAPE_SET).to_string()
}

/// Complete coverage for a request served in full from `snapshot`, carrying the real
/// revisions the answer used — never a fabricated digest.
pub(crate) fn complete_coverage(snapshot: &ReadSnapshot) -> Coverage {
    let revision = snapshot.index.as_ref().map_or_else(
        || snapshot.tree_revision.clone(),
        |index| index.revision.clone(),
    );
    Coverage::Complete {
        state: CoverageCompleteState::Complete,
        scope: CoverageScope::Reach {
            reach: CoverageReach::Request,
        },
        origins: vec![ProviderOrigin {
            provider: ProviderId(RUST_READ_PROVIDER_ID.to_owned()),
            revision,
            tree_revision: snapshot.tree_revision.clone(),
            freshness: Freshness::Current,
            source_revision: snapshot.source_revision.clone(),
        }],
    }
}

fn semantic_coverage(family: FactFamily, snapshot: &ReadSnapshot) -> SemanticCoverage {
    SemanticCoverage(BTreeMap::from([(family, complete_coverage(snapshot))]))
}

/// Finds the symbol a witnessed syntax node belongs to.
///
/// A node's range matches a symbol's declaration range (the whole
/// declaration, including attached docs and attributes) for most nodes, but
/// the item node itself only spans its own bytes, so it matches on
/// `item_range` instead.
fn symbol_for_range(file: &IndexedFile, range: ByteRange) -> Option<&RustSymbol> {
    file.syntax()
        .symbols()
        .iter()
        .find(|symbol| symbol.range == range || symbol.item_range == range)
}

fn node_facets(node: &RustNode) -> Vec<NodeFacet> {
    let mut facets = Vec::new();
    if node.kind.ends_with("_item") || node.kind.ends_with("_declaration") {
        facets.extend([NodeFacet::Declaration, NodeFacet::Definition]);
    }
    if node.kind.ends_with("_expression") {
        facets.push(NodeFacet::Expression);
    }
    if node.kind.ends_with("_statement") {
        facets.push(NodeFacet::Statement);
    }
    if node.kind.contains("comment") {
        facets.push(NodeFacet::Comment);
    }
    facets
}

fn symbol_kind(kind: RustSymbolKind) -> &'static str {
    match kind {
        RustSymbolKind::Function => "rust.function",
        RustSymbolKind::Struct => "rust.struct",
        RustSymbolKind::Enum => "rust.enum",
        RustSymbolKind::Trait => "rust.trait",
        RustSymbolKind::TypeAlias => "rust.type_alias",
        RustSymbolKind::Constant => "rust.constant",
        RustSymbolKind::Static => "rust.static",
        RustSymbolKind::Module => "rust.module",
        RustSymbolKind::Macro => "rust.macro",
    }
}

fn symbol_facets(kind: RustSymbolKind, visibility: &RustVisibility) -> Vec<SymbolFacet> {
    let mut facets = match kind {
        RustSymbolKind::Function => vec![SymbolFacet::Value, SymbolFacet::Callable],
        RustSymbolKind::Struct | RustSymbolKind::Enum | RustSymbolKind::Trait => {
            vec![SymbolFacet::Type]
        }
        RustSymbolKind::TypeAlias => vec![SymbolFacet::Type, SymbolFacet::Alias],
        RustSymbolKind::Module => vec![SymbolFacet::Namespace, SymbolFacet::Module],
        RustSymbolKind::Macro => vec![SymbolFacet::Macro],
        RustSymbolKind::Constant | RustSymbolKind::Static => vec![SymbolFacet::Value],
    };
    if visibility == &RustVisibility::Public {
        facets.push(SymbolFacet::Public);
    }
    facets
}

fn authored_visibility(visibility: &RustVisibility) -> String {
    match visibility {
        RustVisibility::Private => "private".into(),
        RustVisibility::Public => "pub".into(),
        RustVisibility::Restricted(authored) => authored.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use rift_core::SourceVisibility;
    use rift_protocol::read::{
        Digest, GetSymbolParams, NodesParams, NodesResult, ProjectPath, ProjectionId, ReadSnapshot,
        RevisionId,
    };
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::{ReadFault, ReadService, WorkspaceIndexLimits};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub struct Beacon;\nimpl Beacon { pub fn signal(&self) {} }\n",
        )?;
        fs::write(directory.path().join("README.md"), "Beacon docs")?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )?;
        Ok((directory, service))
    }

    const DOCUMENTED_SOURCE: &str = "/// A beacon.\n#[derive(Debug)]\npub struct Beacon;\n";

    fn documented_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), DOCUMENTED_SOURCE)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )?;
        Ok((directory, service))
    }

    /// Exercises every Rust declaration kind, a private and a restricted
    /// visibility, and a comment plus an expression statement.
    const RICH_SOURCE: &str = r#"pub enum Level { Low, High }

pub trait Speaks {
    fn say(&self);
}

pub type Alias = u32;

pub const MAX: u32 = 10;

pub static NAME: &str = "beacon";

pub mod inner {
    pub fn nested() {}
}

macro_rules! noop {
    () => {};
}

struct Hidden;

pub(crate) fn scoped() {}

pub fn compute() -> i32 {
    // lookout marker
    let total = 1 + 2;
    total;
    0
}
"#;

    fn rich_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), RICH_SOURCE)?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )?;
        Ok((directory, service))
    }

    fn any_node_has_facet(result: &NodesResult, facet: &str) -> TestResult<bool> {
        let value = serde_json::to_value(result)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        Ok(nodes.iter().any(|node| {
            node["facets"]
                .as_array()
                .is_some_and(|facets| facets.contains(&json!(facet)))
        }))
    }

    #[test]
    fn nodes_return_typed_rust_facts_from_real_file() -> TestResult {
        let (_directory, service) = fixture()?;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: 5,
            projection: None,
            rev: None,
        })?;
        let value = serde_json::to_value(result)?;

        assert!(
            value["nodes"]
                .as_array()
                .is_some_and(|nodes| !nodes.is_empty())
        );
        assert_eq!(value["nodes"][0]["language"]["name"], "rust");
        assert!(
            value["nodes"][0]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("rift://node/rust/"))
        );
        assert_eq!(
            value["snapshot"]["tree_revision"].as_str().map(str::len),
            Some(8)
        );
        Ok(())
    }

    /// The wire `Digest` truncates to eight characters, but the internal identity computation
    /// it truncates from keeps its full sixty-four-character SHA-256, unchanged from before
    /// this release: freshness and any future identity comparison still work off the strong
    /// hash, not the short wire witness.
    #[test]
    fn workspace_digest_keeps_its_full_hash_before_wire_truncation() -> TestResult {
        let (_directory, service) = fixture()?;
        let full = super::workspace_digest(service.index());
        assert_eq!(full.len(), 64);
        let wire = &service.snapshot().tree_revision.0;
        assert_eq!(wire.len(), 8);
        assert!(full.starts_with(wire.as_str()));
        Ok(())
    }

    #[test]
    fn symbol_read_returns_source_and_stable_identity() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;

        assert_eq!(value["hits"][0]["symbol"]["name"], "Beacon");
        assert_eq!(value["hits"][0]["symbol"]["visibility"], "pub");
        assert!(
            value["hits"][0]["symbol"]["facets"]
                .as_array()
                .is_some_and(|facets| facets.contains(&json!("public")))
        );
        assert!(
            value["hits"][0]["symbol"]["id"]
                .as_str()
                .is_some_and(|id| id.contains("/Beacon"))
        );
        assert_eq!(value["hits"][0]["source"]["text"], "pub struct Beacon;");
        assert_eq!(
            value["hits"][0]["symbol"]["origin"]["unit"],
            json!("rift://source/project/src/lib.rs")
        );
        assert_eq!(value["next_cursor"], Value::Null);
        Ok(())
    }

    /// `complete_coverage` fills its `ProviderOrigin` from the snapshot the answer actually
    /// used; a fabricated all-zero digest here would be a defect.
    #[test]
    fn get_symbol_coverage_origins_carry_the_snapshots_real_revisions() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        let snapshot_tree = value["snapshot"]["tree_revision"].clone();
        let snapshot_source = value["snapshot"]["source_revision"].clone();
        let origin = &value["coverage"]["origins"][0];
        assert_eq!(origin["tree_revision"], snapshot_tree);
        assert_eq!(origin["source_revision"], snapshot_source);
        assert_ne!(origin["tree_revision"], json!("00000000"));
        Ok(())
    }

    /// `complete_coverage` falls back to `tree_revision` when the read consulted no index,
    /// instead of unwrapping the absent `IndexSnapshot`.
    #[test]
    fn complete_coverage_falls_back_to_tree_revision_without_an_index_snapshot() -> TestResult {
        let snapshot = ReadSnapshot {
            tree_revision: Digest("deadbeef".to_owned()),
            index: None,
            source_revision: Digest("cafef00d".to_owned()),
            revision: None,
        };
        let value = serde_json::to_value(super::complete_coverage(&snapshot))?;
        assert_eq!(value["origins"][0]["revision"], json!("deadbeef"));
        assert_eq!(value["origins"][0]["tree_revision"], json!("deadbeef"));
        assert_eq!(value["origins"][0]["source_revision"], json!("cafef00d"));
        Ok(())
    }

    #[test]
    fn unsupported_projection_and_history_are_rejected() -> TestResult {
        let (_directory, service) = fixture()?;
        let projection =
            ProjectionId("rift://projection/prj_aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned());
        let nodes = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: 0,
            projection: Some(projection),
            rev: None,
        });
        assert!(matches!(
            nodes.expect_err("projection must fail").fault(),
            ReadFault::Unsupported { .. }
        ));

        let mut symbol: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        symbol.include_history = true;
        assert!(matches!(
            service
                .get_symbol(&symbol)
                .expect_err("history must fail")
                .fault(),
            ReadFault::Unsupported { .. }
        ));
        Ok(())
    }

    #[test]
    fn nodes_missing_source_is_not_found() -> TestResult {
        let (_directory, service) = fixture()?;
        let missing = service.nodes(NodesParams {
            path: ProjectPath("src/missing.rs".to_owned()),
            position: 0,
            projection: None,
            rev: None,
        });
        assert!(matches!(
            missing.expect_err("missing source must fail").fault(),
            ReadFault::NotFound { .. }
        ));
        Ok(())
    }

    #[test]
    fn invalid_root_preserves_index_error_source() {
        let error = ReadService::build(
            std::path::Path::new("not-a-real-rift-workspace"),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect_err("missing root must fail");

        assert!(matches!(error.fault(), ReadFault::Index(_)));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn nodes_rejects_path_outside_project_root() -> TestResult {
        let (_directory, service) = fixture()?;
        let result = service.nodes(NodesParams {
            path: ProjectPath("/etc/passwd".to_owned()),
            position: 0,
            projection: None,
            rev: None,
        });
        assert!(matches!(
            result.expect_err("absolute path must fail").fault(),
            ReadFault::Invalid { .. }
        ));
        Ok(())
    }

    #[test]
    fn get_symbol_rejects_dependency_scope() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": "Beacon", "scope": "dependencies"}))?;
        assert!(matches!(
            service
                .get_symbol(&params)
                .expect_err("dependency scope must fail")
                .fault(),
            ReadFault::Unsupported { .. }
        ));
        Ok(())
    }

    #[test]
    fn nodes_facets_identify_expression_statement_and_comment() -> TestResult {
        let (_directory, service) = rich_fixture()?;

        let expression_position = RICH_SOURCE
            .find("1 + 2")
            .ok_or("fixture must contain expression")? as u64
            + 2;
        let expression = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: expression_position,
            projection: None,
            rev: None,
        })?;
        assert!(any_node_has_facet(&expression, "expression")?);

        let statement_position = RICH_SOURCE
            .find("total;")
            .ok_or("fixture must contain statement")? as u64;
        let statement = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: statement_position,
            projection: None,
            rev: None,
        })?;
        assert!(any_node_has_facet(&statement, "statement")?);

        let comment_position = RICH_SOURCE
            .find("lookout")
            .ok_or("fixture must contain comment")? as u64;
        let comment = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: comment_position,
            projection: None,
            rev: None,
        })?;
        assert!(any_node_has_facet(&comment, "comment")?);

        Ok(())
    }

    #[test]
    fn storage_fault_renders_path_operation_and_io_in_order() {
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "sealed");
        let error = ReadFault::storage("src/lib.rs", "stage", &io);
        assert_eq!(error.descriptor().code(), "storage_failure");
        let context = error.context();
        let keys: Vec<&str> = context.iter().map(rift_core::ErrorContext::key).collect();
        assert_eq!(keys, ["path", "operation", "io"]);
        assert_eq!(context[0].value(), "src/lib.rs");
        assert_eq!(context[2].value(), "sealed");
    }

    #[test]
    fn nodes_backlink_documented_item_to_its_symbol() -> TestResult {
        let (_directory, service) = documented_fixture()?;
        let position = DOCUMENTED_SOURCE
            .find("Beacon")
            .ok_or("fixture must contain the struct name")? as u64;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position,
            projection: None,
            rev: None,
        })?;
        let value = serde_json::to_value(result)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        let item = nodes
            .iter()
            .find(|node| node["kind"] == "rust.struct_item")
            .ok_or("fixture must witness the struct_item node")?;
        assert!(
            item["symbol"]
                .as_str()
                .is_some_and(|id| id.contains("/Beacon")),
            "documented struct item must backlink to its symbol, got {:?}",
            item["symbol"]
        );
        Ok(())
    }

    #[test]
    fn nodes_backlink_undocumented_item_to_its_symbol() -> TestResult {
        let (_directory, service) = fixture()?;
        let position = "pub struct ".len() as u64;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position,
            projection: None,
            rev: None,
        })?;
        let value = serde_json::to_value(result)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        let item = nodes
            .iter()
            .find(|node| node["kind"] == "rust.struct_item")
            .ok_or("fixture must witness the struct_item node")?;
        assert!(
            item["symbol"]
                .as_str()
                .is_some_and(|id| id.contains("/Beacon")),
            "undocumented struct item must still backlink to its symbol"
        );
        Ok(())
    }

    #[test]
    fn nodes_report_no_symbol_backlink_for_non_symbol_node() -> TestResult {
        let (_directory, service) = fixture()?;
        let position = "pub struct Beacon;\n".len() as u64;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position,
            projection: None,
            rev: None,
        })?;
        let value = serde_json::to_value(result)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        let impl_node = nodes
            .iter()
            .find(|node| node["kind"] == "rust.impl_item")
            .ok_or("fixture must witness the impl_item node")?;
        assert!(
            impl_node["symbol"].is_null(),
            "impl_item is not itself a declared symbol"
        );
        Ok(())
    }

    /// One committed source file, then uncommitted working-tree drift on top
    /// of it, so a revision read and a working-tree read answer differently.
    fn committed_fixture() -> TestResult<TempDir> {
        let directory = tempfile::tempdir()?;
        rift_history::fixture::init(directory.path());
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn beacon() {}\n")?;
        rift_history::fixture::commit_all(directory.path(), "introduce beacon");
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn beacon() -> u8 {\n    7\n}\n",
        )?;
        Ok(directory)
    }

    fn revision_service(
        root: &std::path::Path,
        rev: &str,
    ) -> Result<ReadService, super::ReadError> {
        ReadService::at_revision(
            root,
            &RevisionId(rev.to_owned()),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
    }

    #[test]
    fn revision_read_serves_the_committed_declaration() -> TestResult {
        let directory = committed_fixture()?;
        let service = revision_service(directory.path(), "main")?;
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "beacon"}))?;
        let value = serde_json::to_value(service.get_symbol(&params)?)?;
        assert_eq!(
            value["hits"][0]["source"]["text"], "pub fn beacon() {}",
            "the committed body answers, not the drifted working tree"
        );
        let revision = value["snapshot"]["revision"]
            .as_str()
            .ok_or("a revision read's snapshot must carry the resolved commit")?;
        assert_eq!(revision.len(), 40, "the echo is the full commit id");
        assert!(revision.chars().all(|c| c.is_ascii_hexdigit()));
        Ok(())
    }

    #[test]
    fn revision_snapshot_differs_from_the_drifted_working_tree() -> TestResult {
        let directory = committed_fixture()?;
        let at_head = revision_service(directory.path(), "main")?;
        let working = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )?;
        assert_ne!(
            at_head.snapshot().tree_revision,
            working.snapshot().tree_revision,
            "drifted bytes must produce a different tree digest"
        );
        assert_eq!(
            working.snapshot().revision,
            None,
            "a working-tree read carries no revision echo"
        );
        Ok(())
    }

    #[test]
    fn revision_nodes_list_committed_syntax() -> TestResult {
        let directory = committed_fixture()?;
        let service = revision_service(directory.path(), "main")?;
        let result = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: 8,
            projection: None,
            rev: Some(RevisionId("main".to_owned())),
        })?;
        let value = serde_json::to_value(result)?;
        let nodes = value["nodes"].as_array().ok_or("nodes must be array")?;
        assert!(
            nodes
                .iter()
                .any(|node| node["kind"] == "rust.function_item"),
            "position 8 sits inside the committed `pub fn beacon`"
        );
        Ok(())
    }

    #[test]
    fn revision_read_refuses_an_unversioned_workspace_with_the_actionable_message() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let error = revision_service(directory.path(), "main")
            .expect_err("a workspace without a repository must refuse");
        assert_eq!(error.descriptor().code(), "capability_unavailable");
        let canonical = fs::canonicalize(directory.path())?;
        assert_eq!(
            error.to_string(),
            format!(
                "no configured provider serves this request: workspace {}, \
                 requires a git repository — run `git init`, or omit `rev` to \
                 read the current tree; adjust the request to a served \
                 capability, or configure a provider that serves it",
                canonical.display()
            )
        );
        Ok(())
    }

    #[test]
    fn revision_read_refuses_an_unknown_revision_as_not_found() -> TestResult {
        let directory = committed_fixture()?;
        let error = revision_service(directory.path(), "feature/absent")
            .expect_err("an unknown revision must refuse");
        assert_eq!(error.descriptor().code(), "resource_not_found");
        Ok(())
    }

    #[test]
    fn revision_read_refuses_a_forbidden_spelling_as_invalid() -> TestResult {
        let directory = committed_fixture()?;
        let error = revision_service(directory.path(), "HEAD~1")
            .expect_err("a spelling outside the advertised charset must refuse");
        assert_eq!(
            error.to_string(),
            "the request does not match the documented form: field rev, \
             violation charset_forbidden; correct the reported field and \
             resend the request"
        );
        Ok(())
    }

    #[test]
    fn rev_with_projection_is_refused_as_invalid() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: GetSymbolParams = serde_json::from_value(json!({
            "name": "Beacon",
            "rev": "main",
            "projection": "rift://projection/prj_aaaaaaaaaaaaaaaaaaaaaaaaaa"
        }))?;
        let error = service
            .get_symbol(&params)
            .expect_err("rev with projection must refuse before either is served");
        assert!(matches!(
            error.fault(),
            ReadFault::Invalid { field: "rev", .. }
        ));
        let nodes = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: 0,
            projection: Some(ProjectionId(
                "rift://projection/prj_aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            )),
            rev: Some(RevisionId("main".to_owned())),
        });
        assert!(matches!(
            nodes.expect_err("rev with projection must refuse").fault(),
            ReadFault::Invalid { field: "rev", .. }
        ));
        Ok(())
    }
}
