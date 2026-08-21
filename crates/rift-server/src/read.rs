use std::collections::BTreeMap;
use std::path::Path;

use data_encoding::BASE32_NOPAD;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rift_core::ProjectPath as CoreProjectPath;
use rift_core::constants::{RUST_READ_PROVIDER_ID, SHA256_HEX_LENGTH, SOURCE_UNIT_DIGEST_CHARS};
use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, SourceVisibility};
use rift_index::{
    IndexedFile, SymbolMatch, WorkspaceIndex, WorkspaceIndexError, WorkspaceIndexLimits,
};
use rift_protocol::read::{
    Coverage, CoverageCompleteState, CoverageReach, CoverageScope, Digest, ExactKind, Extensions,
    FactFamily, FileId, Freshness, GetSymbolHit, GetSymbolParams, GetSymbolResult, IndexSnapshot,
    Language, Node, NodeFacet, NodeId, NodesParams, NodesResult, ProviderId, ProviderOrigin,
    ReadSnapshot, SearchScope, SemanticCoverage, SourceExcerpt, SourceKind, SourceLocation,
    SourceUnitId, SourceUnitSpan, Symbol, SymbolFacet, SymbolId, SymbolOrigin, TextRange,
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
            Self::Unsupported { .. } => ErrorName::Wire(ErrorCode::CapabilityUnavailable),
            Self::Invalid { .. } => ErrorName::Wire(ErrorCode::InvalidRequest),
            Self::NotFound { .. } => ErrorName::Wire(ErrorCode::ResourceNotFound),
            Self::Storage { .. } => ErrorName::Wire(ErrorCode::StorageFailure),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::Index(source) => source.context(),
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
}

/// Opaque read-service failure.
pub type ReadError = Error<ReadFault>;

/// Immutable direct-filesystem Rust read service.
#[derive(Debug)]
pub struct ReadService {
    index: WorkspaceIndex,
    snapshot: ReadSnapshot,
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
        let digest = workspace_digest(&index);
        let snapshot = ReadSnapshot {
            tree_revision: Digest(digest.clone()),
            index: Some(IndexSnapshot {
                revision: Digest(digest.clone()),
                tree_revision: Digest(digest.clone()),
                freshness: Freshness::Current,
                source_revision: Digest(digest.clone()),
            }),
            source_revision: Digest(digest),
        };
        Ok(Self { index, snapshot })
    }

    /// Returns the immutable workspace index this snapshot serves.
    pub(crate) const fn index(&self) -> &WorkspaceIndex {
        &self.index
    }

    /// Returns the tree and index revisions captured for this snapshot.
    pub(crate) const fn snapshot(&self) -> &ReadSnapshot {
        &self.snapshot
    }

    /// Reads Rust syntax nodes covering one UTF-8 byte position.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for projections, invalid paths, or missing files.
    pub fn nodes(&self, params: NodesParams) -> Result<NodesResult, ReadError> {
        if params.projection.is_some() {
            return Err(ReadFault::unsupported("projection reads"));
        }
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
            coverage: semantic_coverage(FactFamily::Nodes),
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
            coverage: complete_coverage(),
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
    scope: SearchScope,
) -> Result<(), ReadError> {
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

pub(crate) fn source_unit_id(file: &IndexedFile) -> SourceUnitId {
    let digest = Sha256::digest(file.path().as_str().as_bytes());
    SourceUnitId(format!(
        "rift://source/src_{}",
        digest_prefix_base32(&digest)
    ))
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

/// Renders the leading `SOURCE_UNIT_DIGEST_CHARS` base32 characters of one digest.
///
/// RFC 4648 base32 omits `0`, `1`, `8`, and `9` to avoid confusion with
/// `O`, `I`, `B`, and `g`; source-unit identities use its lowercase form.
/// The witness a node address carries: the first eight lowercase hex
/// characters of the SHA-256 of the node's source bytes. Recomputing it is
/// how resolution proves the bytes behind an address have not drifted.
pub(crate) fn node_witness(source: &str, range: ByteRange) -> String {
    let start = usize::try_from(range.start)
        .unwrap_or(source.len())
        .min(source.len());
    let end = usize::try_from(range.end)
        .unwrap_or(source.len())
        .min(source.len());
    let fingerprint = Sha256::digest(source.get(start..end).unwrap_or_default().as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        fingerprint[0], fingerprint[1], fingerprint[2], fingerprint[3]
    )
}

pub(crate) fn digest_prefix_base32(bytes: &[u8]) -> String {
    let mut encoded = BASE32_NOPAD.encode(bytes).to_ascii_lowercase();
    encoded.truncate(SOURCE_UNIT_DIGEST_CHARS);
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

pub(crate) fn complete_coverage() -> Coverage {
    let revision = Digest("0".repeat(SHA256_HEX_LENGTH));
    Coverage::Complete {
        state: CoverageCompleteState::Complete,
        scope: CoverageScope::Reach {
            reach: CoverageReach::Request,
        },
        origins: vec![ProviderOrigin {
            provider: ProviderId(RUST_READ_PROVIDER_ID.to_owned()),
            revision: revision.clone(),
            tree_revision: revision.clone(),
            freshness: Freshness::Current,
            source_revision: revision,
        }],
    }
}

fn semantic_coverage(family: FactFamily) -> SemanticCoverage {
    SemanticCoverage(BTreeMap::from([(family, complete_coverage())]))
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
        GetSymbolParams, NodesParams, NodesResult, ProjectPath, ProjectionId,
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
            Some(64)
        );
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
        assert_eq!(value["next_cursor"], Value::Null);
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
        })?;
        assert!(any_node_has_facet(&expression, "expression")?);

        let statement_position = RICH_SOURCE
            .find("total;")
            .ok_or("fixture must contain statement")? as u64;
        let statement = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: statement_position,
            projection: None,
        })?;
        assert!(any_node_has_facet(&statement, "statement")?);

        let comment_position = RICH_SOURCE
            .find("lookout")
            .ok_or("fixture must contain comment")? as u64;
        let comment = service.nodes(NodesParams {
            path: ProjectPath("src/lib.rs".to_owned()),
            position: comment_position,
            projection: None,
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
}
