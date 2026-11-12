//! Engine targets named by admitted installed dependency sources.

use std::path::Path;
use std::sync::Arc;

use rift_analysis::{
    ExactPackageInput, ExactPackageLimits, PackageAnalyzer, PackageImportRoot,
    PackageImportRootOrigin, PackageSource,
};
use rift_error::RiftError;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::{Language, Node, SourceKind, SourceUnitId, Symbol, SymbolId};
use rift_protocol::revision::Revision;
use rift_search::sparql::{GraphDatasetBuilder, GraphFact, RevisionIri, SymbolAddress};
use rift_server::{CalleePackage, CalleeRoots, PackageCallee};

use super::relationships::coverage_failure;

pub(super) struct InstalledTarget {
    pub(super) callee: PackageCallee,
    pub(super) symbol: Symbol,
    pub(super) revision: Revision,
    pub(super) unit: SourceUnitId,
    pub(super) source: Arc<str>,
    metadata: Option<String>,
    declarations: Vec<InstalledDeclaration>,
}

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
        let CalleePackage::Installed(package) = callee.package() else {
            return Err(coverage_failure());
        };
        let bytes_max = accepted.providers.syntax.max_file.bytes();
        let (file, source, metadata) = roots.capture_package_call(&callee, &[tree], bytes_max)?;
        let owner = SymbolOwner::Package {
            manager: package.manager.clone(),
            registry: package.registry.clone(),
            name: package.name.clone(),
        };
        let path = file.path().as_str();
        let extension = Path::new(path)
            .extension()
            .and_then(std::ffi::OsStr::to_str);
        let (provider, import_root) =
            match (package.manager.as_str(), file.import_root(), extension) {
                ("pypi", Some(import_root), Some("py")) => {
                    ("python", Some(python_import_root(import_root)?))
                }
                ("npm", None, Some("ts")) if !path.ends_with(".d.ts") => ("typescript", None),
                _ => return Err(coverage_failure()),
            };
        let language = Language {
            name: provider.to_owned(),
            dialect: None,
        };
        let origin =
            rift_core::ContributionOrigin::new(Some(file.origin().clone()), SourceKind::Authored)?;
        let files = [PackageSource::new(file.path(), &source)];
        let limits = ExactPackageLimits::from_configuration(accepted)?;
        let manifest = rift_core::ProjectPath::new("package.json")?;
        let context = metadata
            .as_deref()
            .map(|text| [PackageSource::new(&manifest, text)]);
        let input = ExactPackageInput::new(&owner, &language, &origin, &files, limits)?
            .with_import_roots(import_root.as_slice())?
            .with_framework_context(
                context.as_ref().map_or(&[], |source| source.as_slice()),
                &[],
            )?;
        let analysis = PackageAnalyzer::analyze(input, 1)?;
        if !analysis.warnings().is_empty() {
            return Err(coverage_failure());
        }
        let publication = analysis.publication();
        let offset = u64::try_from(callee.byte_offset(&source)?).map_err(|_| coverage_failure())?;
        let selected = publication
            .declarations
            .iter()
            .filter(|item| item.range.start <= offset && offset < item.range.end)
            .min_by_key(|item| item.range.end - item.range.start)
            .ok_or_else(coverage_failure)?;
        let symbol = publication
            .objects
            .iter()
            .find(|symbol| symbol.id.as_ref() == Some(&selected.symbol))
            .ok_or_else(coverage_failure)?
            .clone();
        let declarations = publication
            .declarations
            .iter()
            .filter(|item| item.symbol == selected.symbol)
            .map(|item| InstalledDeclaration {
                range: item.range.clone(),
                kind: item.node_kind.clone(),
            })
            .collect();
        Ok(Self {
            callee,
            symbol,
            revision: publication.revision.clone(),
            unit: selected.unit.clone(),
            source: source.into(),
            metadata,
            declarations,
        })
    }

    pub(super) fn id(&self) -> Result<&SymbolId, RiftError> {
        self.symbol.id.as_ref().ok_or_else(coverage_failure)
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.source
            .len()
            .saturating_add(self.metadata.as_ref().map_or(0, String::len))
    }

    pub(super) fn same_capture(&self, other: &Self) -> bool {
        self.unit == other.unit
            && self.revision == other.revision
            && self.source == other.source
            && self.metadata == other.metadata
    }

    pub(super) fn verify(&self, roots: &CalleeRoots, tree: &Path) -> Result<(), RiftError> {
        let (file, current, metadata) = roots.capture_package_call(
            &self.callee,
            &[tree],
            u64::try_from(self.source.len()).map_err(|_| coverage_failure())?,
        )?;
        if file.unit().to_string() != self.unit.as_str()
            || current != self.source.as_ref()
            || metadata != self.metadata
        {
            return Err(coverage_failure());
        }
        Ok(())
    }

    pub(super) fn project(
        &self,
        dataset: &mut GraphDatasetBuilder,
        include_node: Option<bool>,
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
        if let Some(include_source) = include_node {
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
                dataset.insert_declared_node(
                    id,
                    &Node {
                        declaration: Some(u64::try_from(index).map_err(|_| coverage_failure())?),
                        index: 0,
                        unit: self.unit.clone(),
                        language: self.symbol.language.clone(),
                        kind: rift_protocol::read::ExactKind(kind),
                        facets: Vec::new(),
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
    let module = import.strip_suffix(".py").unwrap_or(import);
    PackageImportRoot::new(
        None,
        vec![module.replace('/', ".")],
        PackageImportRootOrigin::Wheel,
    )
}
