//! The declarations of one symbol, with exact source and syntax structure.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use rift_core::ProjectPath as CoreProjectPath;
use rift_error::{RiftError, errors};
use rift_index::{IndexedFile, SymbolMatch, WorkspaceIndex, WorkspaceSymbolView};
use rift_protocol::read::{Extensions, FACETS_SOURCE_BYTES_MAX, SymbolId, SyntaxElement};
use rift_syntax::{ByteRange, SyntaxDocument, SyntaxHolder, SyntaxPlace};

/// The address of each declaration of one file, by its position in the file's symbols: the
/// symbol it binds and its ordinal among that symbol's declarations.
pub(crate) type DeclarationAddresses = [Option<(rift_core::SymbolId, usize)>];

/// One page of an answer's facets and its complete result count.
#[derive(Debug)]
pub(crate) struct FacetsPage {
    /// The symbol whose declarations hold the facets.
    pub(crate) symbol: SymbolId,
    /// Declarations of this page, in binding order.
    pub(crate) declarations: Vec<rift_protocol::facets_read::FacetDeclaration>,
    /// Total declarations of this symbol.
    pub(crate) total: usize,
}

/// Bounds one page of a read: its declaration count and its source bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PageBounds {
    /// Most facets in one page.
    pub(crate) facets_max: usize,
    /// Most source bytes the facets of one page carry together.
    pub(crate) source_bytes_max: u64,
}

/// The index a `facets` read numbers facets against: the current publication or a captured
/// view of one.
pub(crate) trait FacetIndex {
    /// The held file at `path` parsed again with its syntax element table.
    fn reparsed(&self, path: &CoreProjectPath) -> Result<Option<SyntaxDocument>, RiftError>;
    /// The symbol and ordinal of the declaration at `position` of `path`'s symbols.
    fn address(
        &self,
        path: &CoreProjectPath,
        position: usize,
    ) -> Result<Option<(rift_core::SymbolId, usize)>, RiftError>;
    /// A bounded page of declarations and the complete binding count.
    fn bindings(
        &self,
        key: &rift_core::SymbolId,
        offset: usize,
    ) -> Result<(Vec<SymbolMatch<'_>>, usize), RiftError>;
}

impl FacetIndex for WorkspaceIndex {
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

    fn bindings(
        &self,
        key: &rift_core::SymbolId,
        offset: usize,
    ) -> Result<(Vec<SymbolMatch<'_>>, usize), RiftError> {
        self.symbols_by_identity_page(key, offset, self.limits().results_max())
    }
}

impl FacetIndex for WorkspaceSymbolView {
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

    fn bindings(
        &self,
        key: &rift_core::SymbolId,
        offset: usize,
    ) -> Result<(Vec<SymbolMatch<'_>>, usize), RiftError> {
        self.symbols_by_identity_page(key, offset, self.results_max())
    }
}

/// One file's parse with every syntax element placed in its declaration or unit.
pub(crate) struct PlacedDocument {
    document: SyntaxDocument,
    places: Vec<SyntaxPlace>,
}

impl PlacedDocument {
    /// Places the syntax elements of `document`, whose declarations at each position of its symbols
    /// carry the address `declarations` names; a declaration without one holds no syntax elements.
    pub(crate) fn new(document: SyntaxDocument, declarations: &DeclarationAddresses) -> Self {
        let places = document.syntax_element_places(|position, _| {
            declarations.get(position).is_some_and(Option::is_some)
        });
        Self { document, places }
    }

    /// Parses the held file at `path` again and places its syntax elements against `index`, refused
    /// when the index holds no parse of the file.
    pub(crate) fn of(index: &impl FacetIndex, path: &CoreProjectPath) -> Result<Self, RiftError> {
        let document = index.reparsed(path)?.ok_or_else(|| {
            errors::server::read_unavailable()
                .operation("facets")
                .detail(format!("the index holds no parse of {}", path.as_str()))
                .error()
        })?;
        let declarations = (0..document.symbols().len())
            .map(|position| index.address(path, position))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::new(document, &declarations))
    }

    fn source_bytes(&self, position: usize) -> u64 {
        self.held_by(SyntaxHolder::Declaration(position))
            .fold(0_u64, |total, (element, _)| {
                total.saturating_add(element.range.end.saturating_sub(element.range.start))
            })
    }

    fn declaration(
        &self,
        file: &IndexedFile,
        ordinal: usize,
        position: usize,
    ) -> Option<rift_protocol::facets_read::FacetDeclaration> {
        let syntax: Vec<_> = self
            .held_by(SyntaxHolder::Declaration(position))
            .map(|(element, place)| {
                wire(
                    file,
                    element.kind,
                    element.range,
                    (Some(ordinal), place.index, place.parent),
                )
            })
            .collect();
        let root = syntax.first()?;
        Some(rift_protocol::facets_read::FacetDeclaration {
            declaration: wire_count(ordinal),
            kind: root.kind.clone(),
            range: root.range.clone(),
            source: rift_protocol::read::SourceExcerpt {
                span: rift_protocol::read::SourceUnitSpan {
                    unit: root.unit.clone(),
                    range: root.range.clone(),
                },
                text: root.source.clone(),
            },
            regions: root.regions.clone(),
            syntax,
        })
    }

    /// The syntax elements `holder` holds, in pre-order, with their places.
    fn held_by(
        &self,
        holder: SyntaxHolder,
    ) -> impl Iterator<Item = (&rift_syntax::SyntaxElement, SyntaxPlace)> {
        self.document
            .syntax_elements()
            .iter()
            .zip(self.places.iter().copied())
            .filter(move |(_, place)| place.holder == holder)
    }
}

/// One page of declarations. Each parsed file is reused within this page.
/// A declaration's syntax tree stays with its source when the page reaches its bound.
pub(crate) fn symbol_page(
    index: &impl FacetIndex,
    (id, key): (&SymbolId, &rift_core::SymbolId),
    offset: usize,
    bounds: PageBounds,
) -> Result<Option<FacetsPage>, RiftError> {
    let (mut bindings, total) = index.bindings(key, offset)?;
    for _ in 0..bounds.facets_max {
        if bindings.len() >= bounds.facets_max || offset.saturating_add(bindings.len()) >= total {
            break;
        }
        let (next, _) = index.bindings(key, offset.saturating_add(bindings.len()))?;
        if next.is_empty() {
            break;
        }
        bindings.extend(next);
    }
    if total == 0 {
        return Ok(None);
    }
    let mut documents: BTreeMap<&CoreProjectPath, PlacedDocument> = BTreeMap::new();
    let mut declarations = Vec::new();
    let mut source_bytes = 0_u64;
    for (position, matched) in bindings.iter().take(bounds.facets_max).enumerate() {
        let ordinal = offset.saturating_add(position);
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
        let bytes = placed.source_bytes(position);
        if source_bytes.saturating_add(bytes) > bounds.source_bytes_max {
            return errors::server::read_unavailable().operation("facets")
                .detail("the page's syntax source exceeds the accepted source byte bound; reduce max_results").fail();
        }
        if bytes > FACETS_SOURCE_BYTES_MAX {
            return errors::server::read_unavailable()
                .operation("facets")
                .detail("the declaration's syntax source exceeds the accepted source byte bound")
                .fail();
        }
        source_bytes = source_bytes.saturating_add(bytes);
        if let Some(declaration) = placed.declaration(matched.file, ordinal, position) {
            declarations.push(declaration);
        }
    }
    Ok(Some(FacetsPage {
        symbol: id.clone(),
        declarations,
        total,
    }))
}

/// The wire syntax element of `kind` spanning `range` in `file`, at its place.
fn wire(
    file: &IndexedFile,
    kind: &'static str,
    range: ByteRange,
    (declaration, index, parent): (Option<usize>, usize, Option<usize>),
) -> SyntaxElement {
    SyntaxElement {
        declaration: declaration.map(wire_count),
        index: wire_count(index),
        unit: super::file_id(file.path()),
        language: file.syntax().language().clone(),
        kind: super::wire_kind(kind),
        facets: super::language_provider(file.syntax().language()).syntax_element_facets(kind),
        range: super::text_range(range),
        source: excerpt(file.source(), range),
        regions: Vec::new(),
        parent: parent.map(wire_count),
        extensions: Extensions(BTreeMap::new()),
    }
}

/// A position or count on the wire. Syntax element tables are bounded far below `u64::MAX`.
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
