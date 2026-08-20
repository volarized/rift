use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use data_encoding::BASE32_NOPAD;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rift_core::ProjectPath as CoreProjectPath;
use rift_core::constants::{
    RUST_READ_PROVIDER_ID, SEARCH_RESULTS_DEFAULT, SHA256_HEX_LENGTH, SOURCE_UNIT_DIGEST_CHARS,
};
use rift_core::{ErrorContext, ErrorDescriptor, ErrorName, ErrorRegistry, render_failure};
use rift_index::{
    IndexedFile, SymbolMatch, SymbolMatchRank, WorkspaceIndex, WorkspaceIndexError,
    WorkspaceIndexLimits,
};
use rift_protocol::read::{
    Coverage, CoverageCompleteState, CoverageReach, CoverageScope, Digest, ExactKind, Extensions,
    FactFamily, File, FileContent, FileId, Freshness, GetSymbolHit, GetSymbolParams,
    GetSymbolResult, IndexSnapshot, Language, MatchedField, Node, NodeFacet, NodeId, NodesParams,
    NodesResult, ProviderId, ProviderOrigin, ReadSnapshot, SearchHit, SearchHitTarget,
    SearchParams, SearchParamsTarget, SearchResult, SearchScope, SemanticCoverage, SourceExcerpt,
    SourceKind, SourceLocation, SourceUnitId, SourceUnitSpan, Symbol, SymbolFacet, SymbolId,
    SymbolOrigin, TextRange,
};
use rift_syntax::{ByteRange, RustNode, RustSymbol, RustSymbolKind, RustVisibility};
use sha2::{Digest as _, Sha256};

/// Stable read-service failure classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadErrorKind {
    /// Workspace could not be indexed.
    Index,
    /// Request uses functionality this release does not serve.
    Unsupported,
    /// Request is invalid for direct workspace reads.
    Invalid,
    /// Requested source does not exist.
    NotFound,
    /// Workspace files could not be read or written.
    Storage,
}

/// Opaque read-service failure.
#[derive(Debug)]
pub struct ReadError {
    kind: ReadErrorKind,
    context: Vec<ErrorContext>,
    source: Option<WorkspaceIndexError>,
}

impl ReadError {
    fn new(kind: ReadErrorKind, context: Vec<ErrorContext>) -> Self {
        Self {
            kind,
            context,
            source: None,
        }
    }

    pub(crate) fn unsupported(capability: &'static str) -> Self {
        Self::new(
            ReadErrorKind::Unsupported,
            vec![ErrorContext::new("capability", capability)],
        )
    }

    pub(crate) fn invalid(field: &'static str, violation: impl Into<String>) -> Self {
        Self::new(
            ReadErrorKind::Invalid,
            vec![
                ErrorContext::new("field", field),
                ErrorContext::new("violation", violation),
            ],
        )
    }

    pub(crate) fn not_found(path: impl Into<String>) -> Self {
        Self::new(
            ReadErrorKind::NotFound,
            vec![ErrorContext::new("path", path)],
        )
    }

    pub(crate) fn storage(
        path: impl Into<String>,
        operation: &'static str,
        io: &std::io::Error,
    ) -> Self {
        Self::new(
            ReadErrorKind::Storage,
            vec![
                ErrorContext::new("path", path),
                ErrorContext::new("operation", operation),
                ErrorContext::new("io", io.to_string()),
            ],
        )
    }

    fn index(source: WorkspaceIndexError) -> Self {
        Self {
            kind: ReadErrorKind::Index,
            context: source.context(),
            source: Some(source),
        }
    }

    /// Returns stable failure classification.
    #[must_use]
    pub const fn kind(&self) -> ReadErrorKind {
        self.kind
    }

    /// Returns canonical registry metadata.
    ///
    /// An indexing failure keeps the classification of the
    /// [`WorkspaceIndexError`] it wraps, so a crossed parse bound surfaces as
    /// `limit_exceeded` rather than as an unclassified failure.
    #[must_use]
    pub fn descriptor(&self) -> ErrorDescriptor {
        match self.kind {
            ReadErrorKind::Index => self.source.as_ref().map_or_else(
                || ErrorRegistry::descriptor(ErrorName::InternalError),
                WorkspaceIndexError::descriptor,
            ),
            ReadErrorKind::Unsupported => {
                ErrorRegistry::descriptor(ErrorName::CapabilityUnavailable)
            }
            ReadErrorKind::Invalid => ErrorRegistry::descriptor(ErrorName::InvalidRequest),
            ReadErrorKind::NotFound => ErrorRegistry::descriptor(ErrorName::ResourceNotFound),
            ReadErrorKind::Storage => ErrorRegistry::descriptor(ErrorName::StorageFailure),
        }
    }

    /// Returns ordered typed context.
    #[must_use]
    pub fn context(&self) -> &[ErrorContext] {
        self.context.as_slice()
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&render_failure(self.descriptor(), &self.context))
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|source| source as _)
    }
}

/// Immutable direct-filesystem Rust read service.
#[derive(Debug)]
pub struct ReadService {
    index: WorkspaceIndex,
    snapshot: ReadSnapshot,
}

impl ReadService {
    /// Builds one in-memory snapshot from real workspace files.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when root cannot be indexed within bounds.
    pub fn build(root: &Path, limits: WorkspaceIndexLimits) -> Result<Self, ReadError> {
        let index = WorkspaceIndex::build(root, limits).map_err(ReadError::index)?;
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

    /// Reads Rust syntax nodes covering one UTF-8 byte position.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for projections, invalid paths, or missing files.
    pub fn nodes(&self, params: NodesParams) -> Result<NodesResult, ReadError> {
        if params.projection.is_some() {
            return Err(ReadError::unsupported("projection reads"));
        }
        let path = CoreProjectPath::new(params.path.0)
            .map_err(|error| ReadError::invalid("path", error.violation().label()))?;
        let file = self
            .index
            .file(&path)
            .ok_or_else(|| ReadError::not_found(path.as_str()))?;
        let nodes = self
            .index
            .nodes(&path, params.position)
            .ok_or_else(|| ReadError::not_found(path.as_str()))?
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
            return Err(ReadError::unsupported("symbol history"));
        }
        let limit = admitted_limit(params.limit)?;
        let hits = self
            .index
            .symbols(&params.name, limit)
            .map_err(ReadError::index)?
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

    /// Searches Rust declarations and source lines.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] for deferred filters, traversal, cursors, projections, or scopes.
    pub fn search(&self, params: &SearchParams) -> Result<SearchResult, ReadError> {
        validate_search(params)?;
        let query = params
            .query
            .as_deref()
            .ok_or_else(|| ReadError::invalid("query", "missing"))?;
        if query.is_empty() {
            return Err(ReadError::invalid("query", "empty"));
        }
        let limit = admitted_limit(params.limit.unwrap_or(SEARCH_RESULTS_DEFAULT as u64))?;
        let mut results = Vec::new();
        if matches!(
            params.target,
            SearchParamsTarget::All | SearchParamsTarget::Symbol
        ) {
            for matched in self.index.symbols(query, limit).map_err(ReadError::index)? {
                results.push(symbol_search_hit(matched));
            }
        }
        if results.len() < limit
            && matches!(
                params.target,
                SearchParamsTarget::All | SearchParamsTarget::File
            )
        {
            for (file, line, text) in self
                .index
                .source_matches(query, limit - results.len())
                .map_err(ReadError::index)?
            {
                results.push(file_search_hit(file, line, text));
            }
        }
        results.truncate(limit);
        Ok(SearchResult {
            snapshot: self.snapshot.clone(),
            coverage: complete_coverage(),
            results,
            next_cursor: None,
        })
    }
}

/// Admits a caller-supplied result limit: positive, and inside this
/// platform's addressable range.
fn admitted_limit(requested: u64) -> Result<usize, ReadError> {
    if requested == 0 {
        return Err(ReadError::invalid("limit", "zero"));
    }
    usize::try_from(requested)
        .map_err(|_| ReadError::invalid("limit", format!("{requested} exceeds this platform")))
}

fn validate_common(cursor: bool, projection: bool, scope: SearchScope) -> Result<(), ReadError> {
    if cursor || projection {
        return Err(ReadError::unsupported("cursor and projection reads"));
    }
    if scope == SearchScope::Dependencies {
        return Err(ReadError::unsupported("dependency reads"));
    }
    Ok(())
}

fn validate_search(params: &SearchParams) -> Result<(), ReadError> {
    validate_common(
        params.cursor.is_some(),
        params.projection.is_some(),
        params.scope,
    )?;
    if params.filter.is_some() || params.paths.is_some() || params.traversal.is_some() {
        return Err(ReadError::unsupported("filtered and traversal search"));
    }
    if params.target == SearchParamsTarget::Node {
        return Err(ReadError::unsupported("node search"));
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

fn wire_symbol(matched: SymbolMatch<'_>) -> Symbol {
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

fn symbol_search_hit(matched: SymbolMatch<'_>) -> SearchHit {
    let score = symbol_match_score(matched.rank);
    SearchHit {
        hit: SearchHitTarget::Symbol {
            symbol: wire_symbol(matched),
        },
        score,
        matched_by: vec![MatchedField::Name],
        relationships: None,
        source: Some(excerpt(matched.file, matched.symbol.range)),
        diagnostics: None,
        span: Some(source_span(matched.file, matched.symbol.range)),
        line: Some(line_number(
            matched.file.source(),
            matched.symbol.range.start,
        )),
        path: None,
        distance: None,
    }
}

fn file_search_hit(file: &IndexedFile, line: usize, text: String) -> SearchHit {
    let start = line_start(file.source(), line);
    let end = start.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
    let range = ByteRange { start, end };
    SearchHit {
        hit: SearchHitTarget::File {
            file: File {
                id: file_id(file),
                content: FileContent::Regular {
                    size: u64::try_from(file.source().len()).unwrap_or(u64::MAX),
                    executable: false,
                },
                languages: vec![rust_language()],
                regions: Vec::new(),
                semantic: true,
            },
        },
        score: 1.0,
        matched_by: vec![MatchedField::Content],
        relationships: None,
        source: Some(SourceExcerpt {
            span: source_span(file, range),
            text,
        }),
        diagnostics: None,
        span: Some(source_span(file, range)),
        line: Some(u64::try_from(line).unwrap_or(u64::MAX)),
        path: None,
        distance: None,
    }
}

fn excerpt(file: &IndexedFile, range: ByteRange) -> SourceExcerpt {
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

fn rust_language() -> Language {
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

fn complete_coverage() -> Coverage {
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

fn symbol_for_range(file: &IndexedFile, range: ByteRange) -> Option<&RustSymbol> {
    file.syntax()
        .symbols()
        .iter()
        .find(|symbol| symbol.range == range)
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

const fn symbol_match_score(rank: SymbolMatchRank) -> f64 {
    match rank {
        SymbolMatchRank::QualifiedExact => 1.0,
        SymbolMatchRank::NameExact => 0.9,
        SymbolMatchRank::QualifiedSuffix => 0.8,
        SymbolMatchRank::Substring => 0.7,
    }
}

fn line_number(source: &str, position: u64) -> u64 {
    let end = usize::try_from(position)
        .unwrap_or(source.len())
        .min(source.len());
    u64::try_from(source[..end].match_indices('\n').count() + 1).unwrap_or(u64::MAX)
}

fn line_start(source: &str, line: usize) -> u64 {
    let mut current = 1_usize;
    for (index, byte) in source.bytes().enumerate() {
        if current == line {
            return u64::try_from(index).unwrap_or(u64::MAX);
        }
        if byte == b'\n' {
            current += 1;
        }
    }
    u64::try_from(source.len()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use rift_core::constants::READ_RESULTS_MAX_DEFAULT;
    use rift_index::SymbolMatchRank;
    use rift_protocol::read::{
        GetSymbolParams, NodesParams, NodesResult, ProjectPath, ProjectionId, SearchParams,
    };
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::{ReadErrorKind, ReadService, WorkspaceIndexLimits, symbol_match_score};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub struct Beacon;\nimpl Beacon { pub fn signal(&self) {} }\n",
        )?;
        fs::write(directory.path().join("README.md"), "Beacon docs")?;
        let service = ReadService::build(directory.path(), WorkspaceIndexLimits::default())?;
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
        let service = ReadService::build(directory.path(), WorkspaceIndexLimits::default())?;
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
    fn search_combines_symbol_and_source_matches_with_limit() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "limit": 3
        }))?;
        let value = serde_json::to_value(service.search(&params)?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["hit"]["target"], "symbol");
        assert_eq!(results[0]["score"], 1.0);
        assert_eq!(results[2]["hit"]["target"], "file");
        assert_eq!(results[2]["line"], 1);
        Ok(())
    }

    #[test]
    fn symbol_scores_preserve_semantic_rank() {
        let scores = [
            symbol_match_score(SymbolMatchRank::QualifiedExact),
            symbol_match_score(SymbolMatchRank::NameExact),
            symbol_match_score(SymbolMatchRank::QualifiedSuffix),
            symbol_match_score(SymbolMatchRank::Substring),
        ];
        assert!(scores.windows(2).all(|pair| pair[0] > pair[1]));
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
        assert_eq!(
            nodes.expect_err("projection must fail").kind(),
            ReadErrorKind::Unsupported
        );

        let mut symbol: GetSymbolParams = serde_json::from_value(json!({"name": "Beacon"}))?;
        symbol.include_history = true;
        assert_eq!(
            service
                .get_symbol(&symbol)
                .expect_err("history must fail")
                .kind(),
            ReadErrorKind::Unsupported
        );
        Ok(())
    }

    #[test]
    fn unsupported_filter_and_missing_source_are_distinct() -> TestResult {
        let (_directory, service) = fixture()?;
        let search: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "filter": {"kind": "field", "field": {"field": "name", "op": "eq", "value": "Beacon"}}
        }))?;
        assert_eq!(
            service
                .search(&search)
                .expect_err("filter must fail")
                .kind(),
            ReadErrorKind::Unsupported
        );

        let missing = service.nodes(NodesParams {
            path: ProjectPath("src/missing.rs".to_owned()),
            position: 0,
            projection: None,
        });
        assert_eq!(
            missing.expect_err("missing source must fail").kind(),
            ReadErrorKind::NotFound
        );
        Ok(())
    }

    #[test]
    fn invalid_root_preserves_index_error_source() {
        let error = ReadService::build(
            std::path::Path::new("not-a-real-rift-workspace"),
            WorkspaceIndexLimits::default(),
        )
        .expect_err("missing root must fail");

        assert_eq!(error.kind(), ReadErrorKind::Index);
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
        assert_eq!(
            result.expect_err("absolute path must fail").kind(),
            ReadErrorKind::Invalid
        );
        Ok(())
    }

    #[test]
    fn get_symbol_rejects_dependency_scope() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": "Beacon", "scope": "dependencies"}))?;
        assert_eq!(
            service
                .get_symbol(&params)
                .expect_err("dependency scope must fail")
                .kind(),
            ReadErrorKind::Unsupported
        );
        Ok(())
    }

    #[test]
    fn search_rejects_cursor_and_node_target() -> TestResult {
        let (_directory, service) = fixture()?;
        let cursor: SearchParams =
            serde_json::from_value(json!({"query": "Beacon", "cursor": "page-two"}))?;
        assert_eq!(
            service
                .search(&cursor)
                .expect_err("cursor reads must fail")
                .kind(),
            ReadErrorKind::Unsupported
        );

        let node_target: SearchParams =
            serde_json::from_value(json!({"query": "Beacon", "target": "node"}))?;
        assert_eq!(
            service
                .search(&node_target)
                .expect_err("node target must fail")
                .kind(),
            ReadErrorKind::Unsupported
        );
        Ok(())
    }

    #[test]
    fn search_requires_query_and_rejects_zero_limit() -> TestResult {
        let (_directory, service) = fixture()?;
        let missing_query: SearchParams = serde_json::from_value(json!({}))?;
        assert_eq!(
            service
                .search(&missing_query)
                .expect_err("missing query must fail")
                .kind(),
            ReadErrorKind::Invalid
        );

        let zero_limit: SearchParams =
            serde_json::from_value(json!({"query": "Beacon", "limit": 0}))?;
        let error = service
            .search(&zero_limit)
            .expect_err("zero limit must fail");
        assert_eq!(error.kind(), ReadErrorKind::Invalid);
        assert_eq!(
            error.to_string(),
            "the request does not match the documented form: field limit, \
             violation zero; correct the reported field and resend the request"
        );
        Ok(())
    }

    #[test]
    fn search_target_file_propagates_oversized_result_limit() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "target": "file",
            "limit": READ_RESULTS_MAX_DEFAULT as u64 + 1
        }))?;
        assert_eq!(
            service
                .search(&params)
                .expect_err("oversized file-target limit must fail")
                .kind(),
            ReadErrorKind::Index
        );
        Ok(())
    }

    #[test]
    fn search_target_symbol_excludes_file_hits() -> TestResult {
        let (_directory, service) = fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "Beacon",
            "target": "symbol",
            "limit": 5
        }))?;
        let value = serde_json::to_value(service.search(&params)?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert!(!results.is_empty());
        assert!(results.iter().all(|hit| hit["hit"]["target"] == "symbol"));
        Ok(())
    }

    #[test]
    fn search_reports_multi_line_file_match_position() -> TestResult {
        let (_directory, service) = rich_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "query": "lookout marker",
            "target": "file"
        }))?;
        let value = serde_json::to_value(service.search(&params)?)?;
        let results = value["results"].as_array().ok_or("results must be array")?;
        assert_eq!(results.len(), 1);
        assert!(results[0]["line"].as_u64().is_some_and(|line| line > 1));
        assert_eq!(results[0]["source"]["text"], "    // lookout marker");
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
    fn search_resolves_every_rust_symbol_kind_and_visibility() -> TestResult {
        let (_directory, service) = rich_fixture()?;
        let cases = [
            ("Level", "rust.enum", None),
            ("Speaks", "rust.trait", None),
            ("Alias", "rust.type_alias", None),
            ("MAX", "rust.constant", None),
            ("NAME", "rust.static", None),
            ("inner", "rust.module", None),
            ("noop", "rust.macro", None),
            ("Hidden", "rust.struct", Some("private")),
            ("scoped", "rust.function", Some("pub(crate)")),
        ];
        for (name, kind, visibility) in cases {
            let params: SearchParams = serde_json::from_value(json!({
                "query": name,
                "target": "symbol",
                "limit": 1
            }))?;
            let value = serde_json::to_value(service.search(&params)?)?;
            let hit = &value["results"][0]["hit"]["symbol"];
            assert_eq!(hit["kind"], kind, "unexpected kind for {name}");
            if let Some(expected_visibility) = visibility {
                assert_eq!(
                    hit["visibility"], expected_visibility,
                    "unexpected visibility for {name}"
                );
                assert!(
                    !hit["facets"]
                        .as_array()
                        .ok_or("facets must be array")?
                        .contains(&json!("public")),
                    "{name} must not carry public facet"
                );
            }
        }
        Ok(())
    }
}
