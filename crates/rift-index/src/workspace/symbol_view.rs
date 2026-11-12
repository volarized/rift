use super::{
    Arc, BTreeMap, DocumentationCollection, DocumentationContentIdentity,
    DocumentationSourceIdentity, IdentifierMatcher, IndexDocument, IndexedFile, Language,
    NormalizedGraph, NotebookFiles, Path, PathBuf, ProjectPath, ReadableSymbol, RelationshipStore,
    RiftError, SymbolDeclarations, SymbolId, SymbolMatch, SymbolOwner, TextSourceFile,
    WorkspaceIndex, WorkspaceIndexLimits, WorkspaceSemantics, errors, symbol_rank,
};

mod retained_size;

#[cfg(test)]
mod tests;

/// Immutable source, semantic and ranking material from one admitted workspace publication.
///
/// Build-only path matchers, weak rebuild caches and operation observations are not retained.
#[derive(Clone, Debug)]
pub struct WorkspaceSymbolView {
    root: PathBuf,
    owner: SymbolOwner,
    files: BTreeMap<ProjectPath, Arc<IndexedFile>>,
    frameworks: BTreeMap<ProjectPath, rift_analysis::ResolvedFramework>,
    identities: BTreeMap<(ProjectPath, usize), SymbolId>,
    text_files: BTreeMap<ProjectPath, Arc<TextSourceFile>>,
    notebooks: NotebookFiles,
    semantics: Arc<WorkspaceSemantics>,
    declarations: Arc<SymbolDeclarations>,
    documentation: Arc<DocumentationCollection>,
    documents: Arc<[IndexDocument]>,
    limits: WorkspaceIndexLimits,
    identity_complete: bool,
    retained_bytes: usize,
}

impl WorkspaceIndex {
    /// Retains the immutable material required by reads of this captured publication.
    ///
    /// # Errors
    /// Returns the captured failure when the canonical directory cannot be built.
    pub fn symbol_view(&self) -> Result<Arc<WorkspaceSymbolView>, RiftError> {
        self.symbol_view
            .get_or_init(|| self.build_symbol_view().map(Arc::new))
            .as_ref()
            .cloned()
            .map_err(Clone::clone)
    }

    fn build_symbol_view(&self) -> Result<WorkspaceSymbolView, RiftError> {
        let declarations = self
            .symbol_declarations
            .get_or_init(|| self.build_symbol_declarations().map(Arc::new))
            .as_ref()
            .map_err(Clone::clone)?;
        let mut view = WorkspaceSymbolView {
            root: self.root.clone(),
            owner: self.owner.clone(),
            files: self.files.clone(),
            frameworks: self
                .files
                .iter()
                .filter_map(|(path, file)| {
                    self.frameworks
                        .for_source(path, &file.digest())
                        .map(|context| (path.clone(), context.clone()))
                })
                .collect(),
            identities: declarations
                .canonical
                .iter()
                .flat_map(|(identity, bindings)| {
                    bindings
                        .iter()
                        .map(move |binding| (binding.clone(), identity.clone()))
                })
                .collect(),
            text_files: self.text_files.clone(),
            notebooks: self.notebooks.clone(),
            semantics: Arc::clone(&self.semantics),
            declarations: Arc::clone(declarations),
            documentation: Arc::clone(&self.documentation),
            documents: Arc::from(self.index_documents()),
            limits: self.limits,
            identity_complete: self.warnings.is_empty(),
            retained_bytes: 0,
        };
        view.retained_bytes = retained_size::measure(&view);
        Ok(view)
    }
}

impl SymbolDeclarations {
    pub(super) fn page(
        &self,
        identity: &SymbolId,
        offset: usize,
        limit: usize,
    ) -> (&[(ProjectPath, usize)], usize) {
        let Some(bindings) = self.canonical.get(identity) else {
            return (&[], 0);
        };
        let total = bindings.len();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        (&bindings[start..end], total)
    }
}

impl WorkspaceSymbolView {
    /// Retains already captured source bytes without adding semantic facts.
    ///
    /// # Errors
    /// Refuses a different root, owner or captured file identity.
    pub fn with_sources(&self, sources: &WorkspaceIndex) -> Result<Self, RiftError> {
        if self.root != sources.root || self.owner != sources.owner {
            return errors::analysis::package_input_identity_invalid().fail();
        }
        for (path, file) in &sources.text_files {
            let conflict = self.text_files.get(path).is_some_and(|held| {
                held.digest() != file.digest() || held.executable() != file.executable()
            }) || self.files.get(path).is_some_and(|held| {
                held.digest() != file.digest() || held.executable() != file.executable()
            });
            if conflict {
                return errors::analysis::package_input_identity_invalid().fail();
            }
        }
        let mut view = self.clone();
        for (path, file) in &sources.text_files {
            view.text_files
                .entry(path.clone())
                .or_insert_with(|| Arc::clone(file));
        }
        view.retained_bytes = retained_size::measure(&view);
        Ok(view)
    }

    /// Root of the captured workspace, independent of the caller's current directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Fixed local owner established before normalization.
    #[must_use]
    pub const fn owner(&self) -> &SymbolOwner {
        &self.owner
    }

    /// Bounds applied to the captured source and facts.
    #[must_use]
    pub const fn limits(&self) -> WorkspaceIndexLimits {
        self.limits
    }

    /// Maximum result count admitted by this publication.
    #[must_use]
    pub const fn results_max(&self) -> usize {
        self.limits.results_max()
    }

    /// Iterates captured source files in their original path order.
    pub fn files(&self) -> impl Iterator<Item = &IndexedFile> {
        self.files.values().map(Arc::as_ref)
    }

    /// Returns the established identity at one captured declaration position.
    #[must_use]
    pub fn symbol_identity_at(&self, path: &ProjectPath, position: usize) -> Option<&SymbolId> {
        self.identities.get(&(path.clone(), position))
    }

    /// The symbol the captured declaration at `position` of `path`'s symbols binds, and that
    /// declaration's ordinal among the symbol's declarations in the order
    /// [`Self::symbols_by_identity_page`] lists them.
    #[must_use]
    pub fn declaration_address(
        &self,
        path: &ProjectPath,
        position: usize,
    ) -> Option<(&SymbolId, usize)> {
        self.declarations.address(path, position)
    }

    /// Captured source file, without any filesystem read or path selection.
    #[must_use]
    pub fn file(&self, path: &ProjectPath) -> Option<&IndexedFile> {
        self.files.get(path).map(Arc::as_ref)
    }

    /// Returns the established object for one captured declaration or item range.
    /// Raw syntax without an established identity remains unaddressed.
    #[must_use]
    pub fn symbol_identity(
        &self,
        path: &ProjectPath,
        range: rift_syntax::ByteRange,
    ) -> Option<&SymbolId> {
        let position = self
            .file(path)?
            .syntax()
            .symbols()
            .iter()
            .position(|symbol| symbol.range == range || symbol.item_range == range)?;
        self.identities.get(&(path.clone(), position))
    }

    /// Returns accepted source bytes from the captured syntax or text catalog.
    #[must_use]
    pub fn source(&self, path: &ProjectPath) -> Option<&str> {
        self.files
            .get(path)
            .map(|file| file.source())
            .or_else(|| self.text_files.get(path).map(|file| file.content()))
    }

    /// Parses captured bytes with their admitted language, framework context and bounds, and
    /// returns the document with its complete node table.
    ///
    /// No path matcher or filesystem read selects the provider. Temporary node material
    /// remains bounded by the captured syntax limits and is not stored after the read.
    ///
    /// # Errors
    /// Returns the captured source or syntax refusal when node reconstruction fails.
    pub fn syntax(
        &self,
        path: &ProjectPath,
    ) -> Result<Option<rift_syntax::SyntaxDocument>, RiftError> {
        let Some(file) = self.file(path) else {
            return Ok(None);
        };
        let absolute = self.root.join(path.as_str());
        let provider = rift_syntax::registry::provider_for_language(file.syntax().language())
            .ok_or_else(|| {
                rift_error::errors::index::workspace_syntax()
                    .path(&absolute)
                    .error()
            })?;
        let mut syntax = super::analyze_source(
            path,
            file.source(),
            &absolute,
            provider,
            self.limits.syntax(),
        )?;
        if let Some(context) = self.frameworks.get(path) {
            let (interpreted, _, _) = context
                .apply(
                    rift_syntax::SyntaxSource {
                        path,
                        text: file.source(),
                    },
                    self.limits.syntax(),
                    syntax,
                )
                .map_err(|error| {
                    rift_error::errors::index::workspace_syntax()
                        .path(&absolute)
                        .cause(error)
                        .error()
                })?;
            syntax = interpreted;
        }
        Ok(Some(syntax))
    }

    /// Captured normalized graph, shared with the original publication.
    #[must_use]
    pub fn normalized_graph(&self) -> &NormalizedGraph {
        self.semantics.graph()
    }

    /// Captured reference adjacency, shared with the original publication.
    #[must_use]
    pub fn relationships(&self) -> &RelationshipStore {
        self.semantics.relationships()
    }

    /// Ranking documents derived from the captured source and declaration facts.
    #[must_use]
    pub fn index_documents(&self) -> &[IndexDocument] {
        &self.documents
    }

    /// Captured documentation facts, including original source ownership.
    #[must_use]
    pub fn documentation(&self) -> &DocumentationCollection {
        &self.documentation
    }

    /// Returns bytes addressed by one captured documentation source or notebook cell.
    #[must_use]
    pub fn documentation_content(&self, identity: &DocumentationContentIdentity) -> Option<&str> {
        let DocumentationSourceIdentity::Project { path } = &identity.source else {
            return None;
        };
        let path = rift_core::ProjectPath::new(path.0.as_str()).ok()?;
        if identity.cell.is_some() {
            return crate::documentation::content(&self.notebooks, identity);
        }
        self.files
            .get(&path)
            .map(|file| file.source())
            .or_else(|| self.text_files.get(&path).map(|file| file.content()))
    }

    /// Resolves an established captured object, including one without source bindings.
    ///
    /// # Errors
    /// Returns the original provider failure when the object could not be assembled.
    pub fn object_by_identity(
        &self,
        identity: &SymbolId,
    ) -> Result<Option<ReadableSymbol>, RiftError> {
        self.declarations
            .assembled
            .get(identity)
            .map(|object| object.as_ref().cloned().map_err(Clone::clone))
            .transpose()
    }

    /// Whether the captured facts establish exact absence for this language.
    #[must_use]
    pub fn identity_coverage_complete(&self, language: &Language) -> bool {
        self.identity_complete
            && !self
                .declarations
                .incomplete_languages
                .contains(&language.identity_segment())
    }

    /// Selects one page from immutable binding order and returns the complete binding count.
    ///
    /// # Errors
    /// Returns the existing result-limit refusal or missing captured binding failure.
    pub fn declarations_by_identity_page(
        &self,
        identity: &SymbolId,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<rift_protocol::symbol_read::SymbolDeclaration>, usize), RiftError> {
        self.validate_result_limit(limit)?;
        let (bindings, total) = self.declarations.page(identity, offset, limit);
        let page = bindings
            .iter()
            .map(|binding| {
                self.declarations
                    .facts
                    .get(binding)
                    .cloned()
                    .ok_or_else(|| errors::index::workspace_provider().error())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((page, total))
    }

    /// Selects physical facts from the same immutable binding page.
    ///
    /// # Errors
    /// Returns the existing result-limit refusal.
    pub fn symbols_by_identity_page(
        &self,
        identity: &SymbolId,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<SymbolMatch<'_>>, usize), RiftError> {
        self.validate_result_limit(limit)?;
        let (bindings, total) = self.declarations.page(identity, offset, limit);
        let page = bindings
            .iter()
            .filter_map(|(path, index)| {
                let file = self.file(path)?;
                let symbol = file.syntax().symbols().get(*index)?;
                let rank = symbol_rank(symbol, &IdentifierMatcher::new(&symbol.qualified_name))?;
                Some(SymbolMatch { file, symbol, rank })
            })
            .collect();
        Ok((page, total))
    }

    fn validate_result_limit(&self, limit: usize) -> Result<(), RiftError> {
        self.limits.validate_result_limit(limit)
    }
}
