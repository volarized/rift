//! The `nodes` read: every node of one symbol, or the nodes covering one position, each node
//! carrying its source text.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use rift_core::ProjectPath as CoreProjectPath;
use rift_error::{RiftError, errors};
use rift_index::{IndexedFile, LocatedNode, SymbolMatch, whole_range};
use rift_protocol::read::{
    Extensions, FILE_NODE_KIND, FileId, Language, NODES_SOURCE_BYTES_MAX, Node, NodeUnit,
    NodesParams, SymbolId,
};
use rift_syntax::{ByteRange, SyntaxDocument};
use sha2::{Digest as _, Sha256};

/// The separator between a node cursor's offset and the tag binding it to one read.
const CURSOR_SEPARATOR: char = '.';

/// Characters of the tag that binds a node cursor to its symbol and tree revision.
const CURSOR_TAG_CHARS: usize = 16;

/// What one `nodes` request selects, once its fields passed validation.
#[derive(Debug)]
pub(crate) enum NodesSelector<'request> {
    /// The nodes covering `position` in the file at `path`, outermost first.
    Position {
        /// Project-relative file.
        path: CoreProjectPath,
        /// UTF-8 byte offset the nodes cover.
        position: u64,
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
    /// another tree refuses here.
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
                    .map_or(Ok(0), |cursor| cursor_offset(cursor, id, revision))?;
                Ok(Self::Symbol { id, key, offset })
            }
            (None, _, _) if params.node_cursor.is_some() => Err(invalid(
                "node_cursor",
                "node_cursor continues a symbol read",
            )),
            (None, Some(path), Some(position)) => {
                let path = CoreProjectPath::new(path.0.clone()).map_err(|error| {
                    errors::server::read_invalid()
                        .field("path")
                        .violation(error.detail())
                        .cause(error)
                        .error()
                })?;
                Ok(Self::Position { path, position })
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

/// The tag binding a cursor to `id` and the tree `revision` names: the first
/// `CURSOR_TAG_CHARS` lowercase hex characters of their SHA-256.
fn cursor_tag(id: &SymbolId, revision: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(id.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(revision.as_bytes());
    let mut tag = data_encoding::HEXLOWER.encode(&hasher.finalize());
    tag.truncate(CURSOR_TAG_CHARS);
    tag
}

/// The cursor that resumes a read of `id` over `revision` at `offset`.
fn cursor(offset: usize, id: &SymbolId, revision: &str) -> String {
    format!("{offset}{CURSOR_SEPARATOR}{}", cursor_tag(id, revision))
}

/// The offset `cursor` resumes at, refused when it was minted for another symbol or tree.
fn cursor_offset(cursor: &str, id: &SymbolId, revision: &str) -> Result<usize, RiftError> {
    let (offset, tag) = cursor
        .split_once(CURSOR_SEPARATOR)
        .ok_or_else(|| invalid("node_cursor", "send the cursor a symbol read returned"))?;
    if tag != cursor_tag(id, revision) {
        return Err(invalid(
            "node_cursor",
            "the cursor belongs to another symbol or tree revision; restart the read without it",
        ));
    }
    offset
        .parse()
        .map_err(|_| invalid("node_cursor", "send the cursor a symbol read returned"))
}

/// One page of a symbol's nodes, and the cursor resuming it when nodes remain.
#[derive(Debug)]
pub(crate) struct NodesPage {
    /// Nodes of this page, outermost first per declaration.
    pub(crate) nodes: Vec<Node>,
    /// Continuation for the nodes past this page.
    pub(crate) node_cursor: Option<String>,
}

/// Bounds one page of a symbol read: its node count and its source bytes.
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

/// The parse of a file the index holds a declaration in, refused when the index no longer
/// holds the file.
pub(crate) fn declared_syntax(
    path: &CoreProjectPath,
    parsed: Result<Option<SyntaxDocument>, RiftError>,
) -> Result<SyntaxDocument, RiftError> {
    parsed?.ok_or_else(|| {
        errors::server::read_unavailable()
            .operation("nodes")
            .detail(format!("the index holds no parse of {}", path.as_str()))
            .error()
    })
}

/// Lists the page of `id`'s nodes starting at `offset`, over its declarations in binding
/// order. `syntax` parses one held file again; each file parses once per page.
///
/// A page stops before the node that would cross `bounds`, and always holds at least one
/// node so that a read makes progress. Work is the declarations' subtrees up to the end of
/// the page: `O(offset + nodes_max)` nodes past the parses.
pub(crate) fn symbol_page(
    declarations: &[SymbolMatch<'_>],
    syntax: impl Fn(&CoreProjectPath) -> Result<SyntaxDocument, RiftError>,
    (id, revision): (&SymbolId, &str),
    offset: usize,
    bounds: PageBounds,
) -> Result<NodesPage, RiftError> {
    let mut documents: BTreeMap<&CoreProjectPath, SyntaxDocument> = BTreeMap::new();
    let mut skipped = 0_usize;
    let mut source_bytes = 0_u64;
    let mut nodes = Vec::new();
    for matched in declarations {
        let path = matched.file.path();
        let document = match documents.entry(path) {
            Entry::Occupied(parsed) => parsed.into_mut(),
            Entry::Vacant(unparsed) => unparsed.insert(syntax(path)?),
        };
        for node in document.declaration_nodes(matched.symbol) {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            let bytes = node.range.end.saturating_sub(node.range.start);
            let crosses = nodes.len() >= bounds.nodes_max
                || source_bytes.saturating_add(bytes) > bounds.source_bytes_max;
            if crosses && !nodes.is_empty() {
                let resume = offset.saturating_add(nodes.len());
                return Ok(NodesPage {
                    nodes,
                    node_cursor: Some(cursor(resume, id, revision)),
                });
            }
            source_bytes = source_bytes.saturating_add(bytes);
            let declares = [matched.symbol.range, matched.symbol.item_range].contains(&node.range);
            nodes.push(located_node(
                matched.file,
                node.kind,
                node.range,
                declares.then(|| id.clone()),
            ));
        }
    }
    Ok(NodesPage {
        nodes,
        node_cursor: None,
    })
}

/// The wire node of `kind` spanning `range` in `file`, declaring `of` when given.
pub(crate) fn located_node(
    file: &IndexedFile,
    kind: &'static str,
    range: ByteRange,
    of: Option<SymbolId>,
) -> Node {
    let language = file.syntax().language();
    let located = LocatedNode::of(language, kind, range, file.source());
    let facets = if kind == FILE_NODE_KIND {
        Vec::new()
    } else {
        super::language_provider(language).node_facets(kind)
    };
    wire(
        language,
        super::file_id(file.path()),
        located,
        file.source(),
        (facets, of),
    )
}

/// The file node of a unit no syntax provider parses: language `text`, the unit's whole
/// source.
pub(crate) fn text_file_node(path: &CoreProjectPath, source: &str) -> Node {
    let language = Language::text();
    let located = LocatedNode::of(&language, FILE_NODE_KIND, whole_range(source), source);
    wire(
        &language,
        super::file_id(path),
        located,
        source,
        (Vec::new(), None),
    )
}

/// The file node of a parsed `file`: it spans the whole unit, outside the grammar's root.
pub(crate) fn file_node(file: &IndexedFile) -> Node {
    located_node(file, FILE_NODE_KIND, whole_range(file.source()), None)
}

fn wire(
    language: &Language,
    unit: FileId,
    located: LocatedNode,
    source: &str,
    (facets, of): (Vec<rift_protocol::read::NodeFacet>, Option<SymbolId>),
) -> Node {
    Node {
        id: located.id,
        of,
        unit: NodeUnit::File(unit),
        language: language.clone(),
        kind: super::wire_kind(located.kind),
        facets,
        range: super::text_range(located.range),
        source: excerpt(source, located.range),
        regions: Vec::new(),
        parent: None,
        extensions: Extensions(BTreeMap::new()),
    }
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
