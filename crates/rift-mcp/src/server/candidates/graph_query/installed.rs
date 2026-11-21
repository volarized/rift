//! Engine targets named by admitted installed dependency sources.

use std::path::Path;
use std::sync::Arc;

use rift_analysis::{PackageImportRoot, PackageImportRootOrigin};
use rift_error::RiftError;
use rift_protocol::read::{SourceUnitId, Symbol, SymbolId, SyntaxElement};
use rift_protocol::revision::Revision;
use rift_search::sparql::{GraphDatasetBuilder, GraphFact, RevisionIri, SymbolAddress};
use rift_server::{CalleeRoots, PackageCallee};

use super::edges::coverage_failure;

mod sources;
use sources::Sources;

#[derive(Debug)]
pub(super) struct InstalledTarget {
    pub(super) callee: PackageCallee,
    pub(super) symbol: Symbol,
    pub(super) revision: Revision,
    pub(super) unit: SourceUnitId,
    pub(super) source: Arc<String>,
    sources: Sources,
    declarations: Vec<InstalledDeclaration>,
}

#[derive(Debug)]
struct InstalledDeclaration {
    range: rift_protocol::read::TextRange,
    kind: Option<String>,
}

impl InstalledTarget {
    pub(super) fn capture(
        callee: PackageCallee,
        roots: &CalleeRoots,
        tree: &Path,
        accepted: &rift_protocol::configuration::WorkspaceConfiguration,
    ) -> Result<Self, RiftError> {
        let mut sources = Sources::capture(&callee, roots, tree, accepted)?;
        let analysis = sources.analyze(&callee, roots, tree)?;
        let source = Arc::clone(
            sources
                .files
                .get(callee.path())
                .ok_or_else(coverage_failure)?,
        );
        let offset = u64::try_from(callee.byte_offset(&source)?).map_err(|_| coverage_failure())?;
        let selected = declaration_at(&analysis, callee.path(), offset)?;
        let publication = analysis.publication();
        let symbol = publication
            .objects
            .iter()
            .find(|symbol| symbol.id.as_ref() == Some(&selected.symbol))
            .ok_or_else(coverage_failure)?
            .clone();
        let declarations = publication
            .declarations
            .iter()
            .filter(|item| item.symbol == selected.symbol && item.unit == selected.unit)
            .map(|item| InstalledDeclaration {
                range: item.range.clone(),
                kind: item.syntax_element_kind.clone(),
            })
            .collect();
        Ok(Self {
            callee,
            symbol,
            revision: publication.revision.clone(),
            unit: selected.unit.clone(),
            source,
            sources,
            declarations,
        })
    }

    pub(super) fn id(&self) -> Result<&SymbolId, RiftError> {
        self.symbol.id.as_ref().ok_or_else(coverage_failure)
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.sources.retained_bytes()
    }

    pub(super) fn same_capture(&self, other: &Self) -> bool {
        self.unit == other.unit
            && self.revision == other.revision
            && self.sources.same_capture(&other.sources)
    }

    pub(super) fn verify(&self, roots: &CalleeRoots, tree: &Path) -> Result<(), RiftError> {
        self.sources.verify(&self.callee, roots, tree)
    }

    pub(super) fn project(
        &self,
        dataset: &mut GraphDatasetBuilder,
        include_syntax_element: Option<bool>,
    ) -> Result<(), RiftError> {
        let id = self.id()?;
        let revision = RevisionIri::parse(&self.revision.to_string())?;
        dataset.insert(GraphFact::SymbolDetails {
            address: SymbolAddress::Established(id),
            name: &self.symbol.name,
            kind: &self.symbol.kind.0,
            language: &self.symbol.language,
            revision: &revision,
        })?;
        if let Some(include_source) = include_syntax_element {
            for (index, declaration) in self.declarations.iter().enumerate() {
                let range = &declaration.range;
                let start = usize::try_from(range.start).map_err(|_| coverage_failure())?;
                let end = usize::try_from(range.end).map_err(|_| coverage_failure())?;
                let kind = declaration.kind.clone().ok_or_else(coverage_failure)?;
                let source = if include_source {
                    self.source
                        .get(start..end)
                        .ok_or_else(coverage_failure)?
                        .to_owned()
                } else {
                    String::new()
                };
                dataset.insert_declared_syntax_element(
                    id,
                    &SyntaxElement {
                        declaration: Some(u64::try_from(index).map_err(|_| coverage_failure())?),
                        index: 0,
                        unit: self.unit.clone(),
                        language: self.symbol.language.clone(),
                        kind: rift_protocol::read::ExactKind(kind),
                        kinds: Vec::new(),
                        range: range.clone(),
                        source,
                        regions: Vec::new(),
                        parent: None,
                        extensions: rift_protocol::read::Extensions::default(),
                    },
                    include_source,
                )?;
            }
        }
        Ok(())
    }
}

fn python_import_root(
    import_root: &rift_core::ProjectPath,
) -> Result<PackageImportRoot, RiftError> {
    let import = import_root.as_str();
    let module = import
        .strip_suffix(".pyi")
        .or_else(|| import.strip_suffix(".py"))
        .unwrap_or(import);
    PackageImportRoot::new(
        None,
        vec![module.replace('/', ".")],
        PackageImportRootOrigin::Wheel,
    )
}

fn declaration_at<'a>(
    analysis: &'a rift_analysis::PackageAnalysis,
    path: &rift_core::ProjectPath,
    offset: u64,
) -> Result<&'a rift_protocol::index::PackageSymbol, RiftError> {
    let file = analysis
        .files()
        .iter()
        .find(|file| file.file().path() == path)
        .ok_or_else(coverage_failure)?;
    let declaration = file
        .file()
        .enclosing_symbol(offset, offset.checked_add(1).ok_or_else(coverage_failure)?)
        .ok_or_else(coverage_failure)?;
    if !declaration
        .name_range
        .is_some_and(|range| range.contains(offset))
        && offset != declaration.item_range.start
    {
        return Err(coverage_failure());
    }
    let identity = file
        .placement()
        .logical_identity(&declaration.qualified_name)
        .ok_or_else(coverage_failure)?;
    let unit = file.placement().unit().to_string();
    analysis
        .publication()
        .declarations
        .iter()
        .find(|item| {
            item.symbol.as_str() == identity.as_str()
                && item.unit.as_str() == unit.as_str()
                && item.range.start <= offset
                && offset < item.range.end
        })
        .ok_or_else(coverage_failure)
}

#[cfg(test)]
mod tests;
