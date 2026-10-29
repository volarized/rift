//! The `nodes` read: every node of one symbol, or the nodes of the declaration or unit holding
//! one position, each node carrying its source text and its place.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use rift_core::ProjectPath as CoreProjectPath;
use rift_error::{RiftError, errors};
use rift_index::{IndexedFile, SymbolMatch, WorkspaceIndex, WorkspaceSymbolView, whole_range};
use rift_protocol::read::{
    Extensions, FILE_NODE_KIND, Language, NODES_SOURCE_BYTES_MAX, Node, NodesParams, SourceUnitId,
    SymbolId,
};
use rift_syntax::{ByteRange, NodeHolder, NodePlace, SyntaxDocument};
use sha2::{Digest as _, Sha256};

/// The separator between a node cursor's offset and the tag binding it to one read.
const CURSOR_SEPARATOR: char = '.';

/// Characters of the tag that binds a node cursor to its symbol or unit and tree revision.
const CURSOR_TAG_CHARS: usize = 16;

/// The address of each declaration of one file, by its position in the file's symbols: the
/// symbol it binds and its ordinal among that symbol's declarations.
pub(crate) type DeclarationAddresses = Vec<Option<(rift_core::SymbolId, usize)>>;

/// What one `nodes` request selects, once its fields passed validation.
#[derive(Debug)]
pub(crate) enum NodesSelector<'request> {
    /// The nodes of the declaration or unit holding `position` in the file at `path`.
    Position {
        /// Project-relative file.
        path: CoreProjectPath,
        /// UTF-8 byte offset inside the declaration or unit.
        position: u64,
        /// Continuation of a preceding read of the same position, checked once the read
        /// knows which symbol or unit answers it.
        cursor: Option<&'request str>,
    },
    /// Every node of `id`, from the `offset`th on.
    Symbol {
        /// The symbol as the caller spelled it.
        id: &'request SymbolId,
        /// The symbol as the index keys it.
        key: rift_core::SymbolId,
        /// Nodes a preceding page already answered.
        offset: usize,
    },
}

impl<'request> NodesSelector<'request> {
    /// Classifies `params` into one selector, refusing every combination the contract
    /// excludes. `revision` names the tree the read serves, so a cursor minted against
    /// another tree refuses.
    pub(crate) fn of(params: &'request NodesParams, revision: &str) -> Result<Self, RiftError> {
        match (&params.symbol, &params.path, params.position) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                Err(invalid("symbol", "symbol excludes path and position"))
            }
            (Some(id), None, None) => {
                let key = rift_core::SymbolId::new(id.as_str())?;
                let offset = params
                    .node_cursor
                    .as_deref()
                    .map_or(Ok(0), |cursor| cursor_offset(cursor, id.as_str(), revision))?;
                Ok(Self::Symbol { id, key, offset })
            }
            (None, Some(path), Some(position)) => {
                let path = CoreProjectPath::new(path.0.clone()).map_err(|error| {
                    errors::server::read_invalid()
                        .field("path")
                        .violation(error.detail())
                        .cause(error)
                        .error()
                })?;
                Ok(Self::Position {
                    path,
                    position,
                    cursor: params.node_cursor.as_deref(),
                })
            }
            (None, None, _) => Err(invalid("path", "nodes selects a symbol or a path")),
            (None, Some(_), None) => Err(invalid("position", "a path read names a position")),
        }
    }
}

fn invalid(field: &'static str, violation: &str) -> RiftError {
    errors::server::read_invalid()
        .field(field)
        .violation(violation)
        .error()
}

/// The tag binding a cursor to the answer `key` names, a symbol or a unit identity, and the
/// tree `revision` names: the first `CURSOR_TAG_CHARS` lowercase hex characters of their
/// SHA-256.
fn cursor_tag(key: &str, revision: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hasher.update([0]);
    hasher.update(revision.as_bytes());
    let mut tag = data_encoding::HEXLOWER.encode(&hasher.finalize());
    tag.truncate(CURSOR_TAG_CHARS);
    tag
}

/// The cursor that resumes the answer `key` names over `revision` at `offset`.
fn cursor(offset: usize, key: &str, revision: &str) -> String {
    format!("{offset}{CURSOR_SEPARATOR}{}", cursor_tag(key, revision))
}

/// The offset `cursor` resumes at, refused when it was minted for another answer or tree.
pub(crate) fn cursor_offset(cursor: &str, key: &str, revision: &str) -> Result<usize, RiftError> {
    let (offset, tag) = cursor
        .split_once(CURSOR_SEPARATOR)
        .ok_or_else(|| invalid("node_cursor", "send the cursor a nodes read returned"))?;
    if tag != cursor_tag(key, revision) {
        return Err(invalid(
            "node_cursor",
            "the cursor belongs to another symbol, unit, or tree revision; restart the read \
             without it",
        ));
    }
    offset
        .parse()
        .map_err(|_| invalid("node_cursor", "send the cursor a nodes read returned"))
}

/// One page of an answer's nodes, and the cursor resuming it when nodes remain.
#[derive(Debug)]
pub(crate) struct NodesPage {
    /// The symbol whose declarations hold the nodes, absent for a unit's nodes.
    pub(crate) symbol: Option<SymbolId>,
    /// Nodes of this page, in pre-order per declaration or unit.
    pub(crate) nodes: Vec<Node>,
    /// Continuation for the nodes past this page.
    pub(crate) node_cursor: Option<String>,
}

/// Bounds one page of a read: its node count and its source bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PageBounds {
    /// Most nodes in one page.
    pub(crate) nodes_max: usize,
    /// Most source bytes the nodes of one page carry together.
    pub(crate) source_bytes_max: u64,
}

impl PageBounds {
    /// The bounds a served read applies: `results_max` nodes, [`NODES_SOURCE_BYTES_MAX`]
    /// source bytes.
    pub(crate) const fn served(results_max: usize) -> Self {
        Self {
            nodes_max: results_max,
            source_bytes_max: NODES_SOURCE_BYTES_MAX,
        }
    }
}

/// The index a `nodes` read numbers nodes against: the current publication or a captured
/// view of one.
pub(crate) trait NodeIndex {
    /// The held parsed file at `path`.
    fn held(&self, path: &CoreProjectPath) -> Option<&IndexedFile>;
    /// The held file at `path` parsed again with its node table.
    fn reparsed(&self, path: &CoreProjectPath) -> Result<Option<SyntaxDocument>, RiftError>;
    /// The symbol and ordinal of the declaration at `position` of `path`'s symbols.
    fn address(
        &self,
        path: &CoreProjectPath,
        position: usize,
    ) -> Result<Option<(rift_core::SymbolId, usize)>, RiftError>;
    /// Every declaration bound to `key`, in binding order, up to the read's result bound.
    fn bindings(&self, key: &rift_core::SymbolId) -> Result<Vec<SymbolMatch<'_>>, RiftError>;
}

impl NodeIndex for WorkspaceIndex {
    fn held(&self, path: &CoreProjectPath) -> Option<&IndexedFile> {
        self.file(path)
    }

    fn reparsed(&self, path: &CoreProjectPath) -> Result<Option<SyntaxDocument>, RiftError> {
        self.syntax(path)
    }

    fn address(
        &self,
        path: &CoreProjectPath,
        position: usize,
    ) -> Result<Option<(rift_core::SymbolId, usize)>, RiftError> {
        self.declaration_address(path, position)
    }

    fn bindings(&self, key: &rift_core::SymbolId) -> Result<Vec<SymbolMatch<'_>>, RiftError> {
        Ok(self
            .symbols_by_identity_page(key, 0, self.limits().results_max())?
            .0)
    }
}

impl NodeIndex for WorkspaceSymbolView {
    fn held(&self, path: &CoreProjectPath) -> Option<&IndexedFile> {
        self.file(path)
    }

    fn reparsed(&self, path: &CoreProjectPath) -> Result<Option<SyntaxDocument>, RiftError> {
        self.syntax(path)
    }

    fn address(
        &self,
        path: &CoreProjectPath,
        position: usize,
    ) -> Result<Option<(rift_core::SymbolId, usize)>, RiftError> {
        Ok(self
            .declaration_address(path, position)
            .map(|(identity, ordinal)| (identity.clone(), ordinal)))
    }

    fn bindings(&self, key: &rift_core::SymbolId) -> Result<Vec<SymbolMatch<'_>>, RiftError> {
        Ok(self.symbols_by_identity_page(key, 0, self.results_max())?.0)
    }
}

/// One file's parse with every node placed in its declaration or unit.
pub(crate) struct PlacedDocument {
    document: SyntaxDocument,
    places: Vec<NodePlace>,
    declarations: DeclarationAddresses,
}

impl PlacedDocument {
    /// Places the nodes of `document`, whose declarations at each position of its symbols
    /// carry the address `declarations` names; a declaration without one holds no nodes.
    pub(crate) fn new(document: SyntaxDocument, declarations: DeclarationAddresses) -> Self {
        let places = document
            .node_places(|position, _| declarations.get(position).is_some_and(Option::is_some));
        Self {
            document,
            places,
            declarations,
        }
    }

    /// Parses the held file at `path` again and places its nodes against `index`, refused
    /// when the index holds no parse of the file.
    pub(crate) fn of(index: &impl NodeIndex, path: &CoreProjectPath) -> Result<Self, RiftError> {
        let document = index.reparsed(path)?.ok_or_else(|| {
            errors::server::read_unavailable()
                .operation("nodes")
                .detail(format!("the index holds no parse of {}", path.as_str()))
                .error()
        })?;
        let declarations = (0..document.symbols().len())
            .map(|position| index.address(path, position))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::new(document, declarations))
    }

    /// The symbol whose declaration holds the innermost node covering `position`; `None`
    /// when the unit holds it.
    pub(crate) fn holder_at(&self, position: u64) -> Option<&rift_core::SymbolId> {
        let innermost = self
            .document
            .nodes()
            .iter()
            .rposition(|node| node.range.contains(position))?;
        match self.places[innermost].holder {
            NodeHolder::Declaration(declaration) => self
                .declarations
                .get(declaration)
                .and_then(Option::as_ref)
                .map(|(symbol, _)| symbol),
            NodeHolder::Unit => None,
        }
    }

    /// The nodes `holder` holds, in pre-order, with their places.
    fn held_by(
        &self,
        holder: NodeHolder,
    ) -> impl Iterator<Item = (&rift_syntax::SyntaxNode, NodePlace)> {
        self.document
            .nodes()
            .iter()
            .zip(self.places.iter().copied())
            .filter(move |(_, place)| place.holder == holder)
    }
}

/// Collects one page: skips `offset` candidates, then takes nodes until the next would
/// cross `bounds`, always keeping at least one so that a read makes progress.
struct PageBuilder<'key> {
    key: &'key str,
    revision: &'key str,
    offset: usize,
    bounds: PageBounds,
    skipped: usize,
    source_bytes: u64,
    nodes: Vec<Node>,
}

impl<'key> PageBuilder<'key> {
    const fn new(key: &'key str, revision: &'key str, offset: usize, bounds: PageBounds) -> Self {
        Self {
            key,
            revision,
            offset,
            bounds,
            skipped: 0,
            source_bytes: 0,
            nodes: Vec::new(),
        }
    }

    /// Offers the next candidate; `Some(cursor)` when the page is full before it.
    fn offer(&mut self, range: ByteRange, node: impl FnOnce() -> Node) -> Option<String> {
        if self.skipped < self.offset {
            self.skipped += 1;
            return None;
        }
        let bytes = range.end.saturating_sub(range.start);
        let crosses = self.nodes.len() >= self.bounds.nodes_max
            || self.source_bytes.saturating_add(bytes) > self.bounds.source_bytes_max;
        if crosses && !self.nodes.is_empty() {
            let resume = self.offset.saturating_add(self.nodes.len());
            return Some(cursor(resume, self.key, self.revision));
        }
        self.source_bytes = self.source_bytes.saturating_add(bytes);
        self.nodes.push(node());
        None
    }
}

/// One page of `id`'s nodes, over every declaration `index` binds to it in binding order;
/// `None` when no declaration binds it. Each file parses once per page.
///
/// A page stops before the node that would cross `bounds`, and always holds at least one
/// node. Work is the declarations' files' node tables up to the end of the page.
pub(crate) fn symbol_page(
    index: &impl NodeIndex,
    (id, key): (&SymbolId, &rift_core::SymbolId),
    (offset, revision): (usize, &str),
    bounds: PageBounds,
) -> Result<Option<NodesPage>, RiftError> {
    let declarations = index.bindings(key)?;
    if declarations.is_empty() {
        return Ok(None);
    }
    let mut documents: BTreeMap<&CoreProjectPath, PlacedDocument> = BTreeMap::new();
    let mut page = PageBuilder::new(id.as_str(), revision, offset, bounds);
    for (ordinal, matched) in declarations.iter().enumerate() {
        let path = matched.file.path();
        let placed = match documents.entry(path) {
            Entry::Occupied(parsed) => parsed.into_mut(),
            Entry::Vacant(unparsed) => unparsed.insert(PlacedDocument::of(index, path)?),
        };
        let Some(position) = matched
            .file
            .syntax()
            .symbols()
            .iter()
            .position(|symbol| std::ptr::eq(symbol, matched.symbol))
        else {
            continue;
        };
        let declaration = placed
            .declarations
            .get(position)
            .and_then(Option::as_ref)
            .map_or(ordinal, |(_, ordinal)| *ordinal);
        for (node, place) in placed.held_by(NodeHolder::Declaration(position)) {
            let wired = || {
                wire(
                    matched.file,
                    node.kind,
                    node.range,
                    (Some(declaration), place.index, place.parent),
                )
            };
            if let Some(cursor) = page.offer(node.range, wired) {
                return Ok(Some(NodesPage {
                    symbol: Some(id.clone()),
                    nodes: page.nodes,
                    node_cursor: Some(cursor),
                }));
            }
        }
    }
    Ok(Some(NodesPage {
        symbol: Some(id.clone()),
        nodes: page.nodes,
        node_cursor: None,
    }))
}

/// One page of the nodes of the declaration holding `position` in the held file at `path`:
/// every node of that declaration's symbol, or the file's unit's nodes when no declaration
/// holds the position. `None` when `index` holds no parsed file at `path`. A `cursor`
/// continues the answer it was minted for.
pub(crate) fn position_page(
    index: &impl NodeIndex,
    (path, position): (&CoreProjectPath, u64),
    (cursor, revision): (Option<&str>, &str),
    bounds: PageBounds,
) -> Result<Option<NodesPage>, RiftError> {
    let Some(file) = index.held(path) else {
        return Ok(None);
    };
    let placed = PlacedDocument::of(index, path)?;
    if let Some(symbol) = placed.holder_at(position).cloned() {
        let id = SymbolId(symbol.as_str().to_owned());
        let offset = cursor.map_or(Ok(0), |cursor| cursor_offset(cursor, id.as_str(), revision))?;
        if let Some(page) = symbol_page(index, (&id, &symbol), (offset, revision), bounds)? {
            return Ok(Some(page));
        }
    }
    let unit = super::file_id(path);
    let source = UnitSource {
        unit: &unit,
        language: file.syntax().language(),
        source: file.source(),
    };
    unit_page(source, Some(&placed), cursor, revision, bounds).map(Some)
}

/// One page of the nodes `file`'s unit holds: its file node, then every node of `placed`
/// no declaration holds. A unit no syntax provider parses holds its file node alone.
pub(crate) fn unit_page(
    file: UnitSource<'_>,
    placed: Option<&PlacedDocument>,
    cursor: Option<&str>,
    revision: &str,
    bounds: PageBounds,
) -> Result<NodesPage, RiftError> {
    let key = file.unit.0.clone();
    let offset = cursor.map_or(Ok(0), |cursor| cursor_offset(cursor, &key, revision))?;
    let mut page = PageBuilder::new(&key, revision, offset, bounds);
    let whole = whole_range(file.source);
    let file_node = || file.node(FILE_NODE_KIND, whole, (None, 0, None));
    if let Some(cursor) = page.offer(whole, file_node) {
        return Ok(NodesPage {
            symbol: None,
            nodes: page.nodes,
            node_cursor: Some(cursor),
        });
    }
    for (node, place) in placed
        .into_iter()
        .flat_map(|placed| placed.held_by(NodeHolder::Unit))
    {
        let wired = || file.node(node.kind, node.range, (None, place.index, place.parent));
        if let Some(cursor) = page.offer(node.range, wired) {
            return Ok(NodesPage {
                symbol: None,
                nodes: page.nodes,
                node_cursor: Some(cursor),
            });
        }
    }
    Ok(NodesPage {
        symbol: None,
        nodes: page.nodes,
        node_cursor: None,
    })
}

/// The unit a unit read lists: its identity, language, and source.
#[derive(Clone, Copy)]
pub(crate) struct UnitSource<'file> {
    /// The unit's file identity.
    pub(crate) unit: &'file SourceUnitId,
    /// The grammar that parsed the unit, or `text` for a unit no provider parses.
    pub(crate) language: &'file Language,
    /// The unit's source.
    pub(crate) source: &'file str,
}

impl UnitSource<'_> {
    fn node(
        &self,
        kind: &'static str,
        range: ByteRange,
        (declaration, index, parent): (Option<usize>, usize, Option<usize>),
    ) -> Node {
        let facets = if kind == FILE_NODE_KIND {
            Vec::new()
        } else {
            super::language_provider(self.language).node_facets(kind)
        };
        Node {
            declaration: declaration.map(wire_count),
            index: wire_count(index),
            unit: self.unit.clone(),
            language: self.language.clone(),
            kind: super::wire_kind(kind),
            facets,
            range: super::text_range(range),
            source: excerpt(self.source, range),
            regions: Vec::new(),
            parent: parent.map(wire_count),
            extensions: Extensions(BTreeMap::new()),
        }
    }
}

/// The wire node of `kind` spanning `range` in `file`, at its place.
fn wire(
    file: &IndexedFile,
    kind: &'static str,
    range: ByteRange,
    place: (Option<usize>, usize, Option<usize>),
) -> Node {
    let unit = super::file_id(file.path());
    UnitSource {
        unit: &unit,
        language: file.syntax().language(),
        source: file.source(),
    }
    .node(kind, range, place)
}

/// A position or count on the wire. Node tables are bounded far below `u64::MAX`.
fn wire_count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// The bytes `range` spans in `source`, which the range was minted from.
fn excerpt(source: &str, range: ByteRange) -> String {
    usize::try_from(range.start)
        .ok()
        .zip(usize::try_from(range.end).ok())
        .and_then(|(start, end)| source.get(start..end))
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests;
