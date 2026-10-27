use std::collections::{BTreeMap, BTreeSet};

use rift_error::{RiftError, errors};
use rift_protocol::identity::{
    SYMBOL_ID_BYTES_MAX, SymbolIdentity, SymbolOwner, parse_local_scope,
};
use rift_protocol::read::Language;
use rift_syntax::SyntaxLimits;

use super::{
    ArchiveMemberKind, ExactPackageInput, ExactPackageLimits, PackageImportRoot, PackageSource,
};

/// Captured owner, selected sources, and metadata used to place logical declarations.
#[derive(Debug, Clone, Copy)]
pub struct NamespaceInput<'input> {
    owner: &'input SymbolOwner,
    language: &'input Language,
    files: &'input [PackageSource<'input>],
    context_sources: &'input [PackageSource<'input>],
    import_roots: &'input [PackageImportRoot],
    files_max: usize,
    bytes_max: u64,
    syntax: SyntaxLimits,
    library: Option<(PackageSource<'input>, PackageSource<'input>)>,
    modules: &'input [NamespaceModule<'input>],
    dependencies: &'input [super::PackageDependency<'input>],
    build_paths:
        Option<&'input BTreeMap<rift_protocol::read::ProjectPath, Option<ArchiveMemberKind>>>,
}

impl<'input> NamespaceInput<'input> {
    pub(super) fn from_package(input: &ExactPackageInput<'input>) -> Self {
        Self {
            owner: input.owner,
            language: input.language,
            files: input.files,
            context_sources: input.context_sources,
            import_roots: input.import_roots,
            files_max: input
                .limits
                .files_max()
                .min(input.limits.publication().units) as usize,
            bytes_max: input.limits.bytes_max(),
            syntax: input.limits.syntax(),
            library: input.library,
            modules: input.modules,
            dependencies: input.dependencies,
            build_paths: input.build_paths,
        }
    }

    /// Accepts a fixed local owner and its captured source and metadata inventory.
    ///
    /// The caller supplies the owner from the selected root or accepted registration.
    /// Metadata and selected source bytes share the input count and byte bounds.
    ///
    /// # Errors
    /// Returns the existing input refusal for invalid scope, sources, or import roots.
    pub fn project(
        owner: &'input SymbolOwner,
        language: &'input Language,
        files: &'input [PackageSource<'input>],
        context_sources: &'input [PackageSource<'input>],
        import_roots: &'input [PackageImportRoot],
        limits: ExactPackageLimits,
    ) -> Result<Self, RiftError> {
        Self::workspace(
            owner,
            language,
            files,
            context_sources,
            import_roots,
            (
                limits.files_max() as usize,
                limits.bytes_max(),
                limits.syntax(),
            ),
        )
    }

    /// Accepts captured local sources under the workspace count, byte, and syntax bounds.
    ///
    /// # Errors
    /// Returns the existing input refusal for invalid scope, sources, roots, or zero bounds.
    pub fn workspace(
        owner: &'input SymbolOwner,
        language: &'input Language,
        files: &'input [PackageSource<'input>],
        context_sources: &'input [PackageSource<'input>],
        import_roots: &'input [PackageImportRoot],
        bounds: (usize, u64, SyntaxLimits),
    ) -> Result<Self, RiftError> {
        let (files_max, bytes_max, syntax) = bounds;
        if files_max == 0 || bytes_max == 0 {
            return Err(invalid());
        }
        let scope = match owner {
            SymbolOwner::Local => "local".to_owned(),
            SymbolOwner::NamedLocal { name } => {
                owner.validate().map_err(|_| invalid())?;
                format!("local@{name}")
            }
            SymbolOwner::ProfileLocal { .. } => owner.local_scope().map_err(|_| invalid())?,
            _ => return Err(invalid()),
        };
        if parse_local_scope(&scope).map_err(|_| invalid())? != *owner {
            return Err(invalid());
        }
        super::validate_source_count_at_bound(
            files.len().saturating_add(context_sources.len()),
            files_max,
        )?;
        let combined = files
            .iter()
            .chain(context_sources)
            .copied()
            .collect::<Vec<_>>();
        super::validate_source_bytes_and_units(&combined, bytes_max, |file| match file.physical {
            Some(_) => super::released_source_unit(owner, file),
            None => rift_syntax::source_unit_for_path(file.path),
        })?;
        super::import_root::validate_roots(import_roots)?;
        super::import_root::validate_runtime_roots(owner, language, files, import_roots)?;
        Ok(Self {
            owner,
            language,
            files,
            context_sources,
            import_roots,
            files_max,
            bytes_max,
            syntax,
            library: None,
            modules: &[],
            dependencies: &[],
            build_paths: None,
        })
    }

    /// Adds dependency witnesses without copying their foreign objects.
    ///
    /// Each selector must occur in an original admitted Cargo manifest. Missing or
    /// conflicting target declarations remain unresolved during requested lookup.
    ///
    /// # Errors
    /// Returns the existing input refusal for invalid witnesses or exceeded bounds.
    pub fn with_dependencies(
        mut self,
        dependencies: &'input [super::PackageDependency<'input>],
    ) -> Result<Self, RiftError> {
        self.validate_count(dependencies.len())?;
        let mut names = BTreeSet::new();
        let mut sources = self
            .files
            .len()
            .checked_add(self.context_sources.len())
            .ok_or_else(invalid)?;
        let mut bytes = self
            .files
            .iter()
            .chain(self.context_sources)
            .try_fold(0_u64, |bytes, source| {
                counted_bytes(bytes, source.text.len(), self.bytes_max)
            })?;
        for dependency in dependencies {
            bytes = counted_bytes(bytes, dependency.name().len(), self.bytes_max)?;
            if !names.insert(dependency.name()) {
                return Err(invalid());
            }
            sources = sources
                .checked_add(dependency.sources().len())
                .ok_or_else(invalid)?;
            self.validate_count(sources)?;
            for source in dependency.sources() {
                bytes = counted_bytes(bytes, source.text().len(), self.bytes_max)?;
            }
            dependency.validate(&self)?;
        }
        self.dependencies = dependencies;
        Ok(self)
    }

    /// Borrowed admitted dependency publications, in caller order.
    #[must_use]
    pub const fn dependencies(self) -> &'input [super::PackageDependency<'input>] {
        self.dependencies
    }

    /// Adds selected module observations without combining their export conditions.
    ///
    /// Every source must match the admitted selected inventory. Observation strings
    /// share the source byte bound; references and conditions share the source count bound.
    /// Language helpers validate the supplied observation against captured metadata and syntax.
    ///
    /// # Errors
    /// Returns the existing input refusal for missing witnesses or exceeded bounds.
    pub fn with_modules(
        mut self,
        modules: &'input [NamespaceModule<'input>],
    ) -> Result<Self, RiftError> {
        validate_modules(&self, modules)?;
        self.modules = modules;
        Ok(self)
    }

    /// Selected module observations and their separate export conditions.
    #[must_use]
    pub const fn modules(self) -> &'input [NamespaceModule<'input>] {
        self.modules
    }

    /// Adds captured build-path observations under the input count and byte bounds.
    ///
    /// An absent key is unknown. `None` records a verified missing path; other values
    /// retain the file type observed without following a symbolic link.
    ///
    /// # Errors
    /// Returns the existing input refusal for invalid paths or exceeded bounds.
    pub fn with_build_paths(
        mut self,
        paths: &'input BTreeMap<rift_protocol::read::ProjectPath, Option<ArchiveMemberKind>>,
    ) -> Result<Self, RiftError> {
        let count = self
            .files
            .len()
            .checked_add(self.context_sources.len())
            .and_then(|count| count.checked_add(paths.len()))
            .ok_or_else(invalid)?;
        self.validate_count(count)?;
        let mut bytes = self
            .files
            .iter()
            .chain(self.context_sources)
            .try_fold(0_u64, |bytes, source| {
                counted_bytes(bytes, source.text.len(), self.bytes_max)
            })?;
        for path in paths.keys() {
            rift_core::ProjectPath::new(path.0.as_str()).map_err(|_| invalid())?;
            bytes = counted_bytes(bytes, path.0.len(), self.bytes_max)?;
        }
        self.build_paths = Some(paths);
        Ok(self)
    }

    /// Captures an explicitly selected non-test Cargo library acquisition.
    ///
    /// # Errors
    /// Returns the existing input refusal unless owner, manifest, and library root
    /// match the original admitted sources and parsed library target.
    pub fn with_library(
        mut self,
        owner: &SymbolOwner,
        manifest: PackageSource<'input>,
        root: PackageSource<'input>,
    ) -> Result<Self, RiftError> {
        if owner != self.owner || self.language.name != "rust" || self.language.dialect.is_some() {
            return Err(invalid());
        }
        for (source, admitted) in [
            (
                manifest,
                self.files
                    .iter()
                    .chain(self.context_sources)
                    .find(|file| file.path == manifest.path),
            ),
            (root, self.files.iter().find(|file| file.path == root.path)),
        ] {
            let Some(admitted) = admitted else {
                return Err(invalid());
            };
            if source.text != admitted.text || source.physical != admitted.physical {
                return Err(invalid());
            }
        }
        let files = [root];
        let context = [manifest];
        let selected = NamespaceInput {
            files: &files,
            context_sources: &context,
            library: None,
            ..self
        };
        if crate::analyzer::namespace::library(&selected)
            .is_none_or(|(path, _)| path != root.path.as_str())
        {
            return Err(invalid());
        }
        self.library = Some((manifest, root));
        Ok(self)
    }

    /// Explicitly selected non-test library sources, or unknown acquisition context.
    #[must_use]
    pub const fn library(self) -> Option<(PackageSource<'input>, PackageSource<'input>)> {
        self.library
    }

    /// Returns the Cargo library target established by the original package manifest.
    #[must_use]
    pub fn library_target(self) -> Option<(rift_core::ProjectPath, String)> {
        let (path, name) = crate::analyzer::namespace::library(&self)?;
        Some((rift_core::ProjectPath::new(path).ok()?, name))
    }

    /// Captured build-path observations, or no supplied observations.
    #[must_use]
    pub const fn build_paths(
        self,
    ) -> Option<&'input BTreeMap<rift_protocol::read::ProjectPath, Option<ArchiveMemberKind>>> {
        self.build_paths
    }

    /// Logical owner selected before namespace placement.
    #[must_use]
    pub const fn owner(self) -> &'input SymbolOwner {
        self.owner
    }

    /// Language selected by the caller.
    #[must_use]
    pub const fn language(self) -> &'input Language {
        self.language
    }

    /// Captured selected source files.
    #[must_use]
    pub const fn files(self) -> &'input [PackageSource<'input>] {
        self.files
    }

    /// Captured metadata sources.
    #[must_use]
    pub const fn context_sources(self) -> &'input [PackageSource<'input>] {
        self.context_sources
    }

    /// Validated import roots.
    #[must_use]
    pub const fn import_roots(self) -> &'input [PackageImportRoot] {
        self.import_roots
    }

    /// Syntax bounds used for selected sources and captured metadata.
    #[must_use]
    pub const fn syntax(self) -> SyntaxLimits {
        self.syntax
    }

    /// Returns captured Cargo targets and literal module paths needed by selected declarations.
    ///
    /// The supplied inventory contains paths from the same committed tree. Returned paths
    /// request source witnesses; they do not establish object identities.
    ///
    /// # Errors
    /// Returns the existing input refusal when requested source paths exceed the input bound.
    pub fn namespace_paths(
        self,
        documents: &[&crate::IndexedFile],
        contains: &dyn Fn(&str) -> bool,
        selected: &std::collections::BTreeSet<rift_core::ProjectPath>,
    ) -> Result<std::collections::BTreeSet<rift_core::ProjectPath>, RiftError> {
        self.validate_count(documents.len())?;
        if selected.is_empty() {
            return Ok(std::collections::BTreeSet::new());
        }
        crate::analyzer::namespace::namespace_paths(
            &self,
            documents,
            contains,
            selected,
            self.files_max,
        )
    }

    /// Requests the bounded literal module closure needed for captured macro scope.
    ///
    /// Paths retain the selected package's spelling. This only requests original source
    /// witnesses; namespace placement still validates those bytes during assembly.
    ///
    /// # Errors
    /// Returns the existing input refusal when source observations exceed the input bound.
    pub fn namespace_closure_paths(
        self,
        documents: &[&crate::IndexedFile],
        contains: &dyn Fn(&str) -> bool,
    ) -> Result<std::collections::BTreeSet<rift_core::ProjectPath>, RiftError> {
        self.validate_count(documents.len())?;
        crate::analyzer::namespace::namespace_paths(
            &self,
            documents,
            contains,
            &std::collections::BTreeSet::new(),
            self.files_max,
        )
    }

    fn validate_count(self, observed: usize) -> Result<(), RiftError> {
        super::validate_source_count_at_bound(observed, self.files_max)
    }
}

/// One selected module observation with its original source witnesses.
#[derive(Debug, Clone, Copy)]
pub struct NamespaceModule<'input> {
    module: &'input str,
    implementation: PackageSource<'input>,
    declarations: &'input [PackageSource<'input>],
    export_conditions: &'input [&'input str],
}

impl<'input> NamespaceModule<'input> {
    /// Records one observation; `NamespaceInput::with_modules` validates its witnesses.
    #[must_use]
    pub const fn new(
        module: &'input str,
        implementation: PackageSource<'input>,
        declarations: &'input [PackageSource<'input>],
        export_conditions: &'input [&'input str],
    ) -> Self {
        Self {
            module,
            implementation,
            declarations,
            export_conditions,
        }
    }

    /// Module name supplied by the captured package or runtime policy.
    #[must_use]
    pub const fn module(self) -> &'input str {
        self.module
    }

    /// Selected implementation source.
    #[must_use]
    pub const fn implementation(self) -> PackageSource<'input> {
        self.implementation
    }

    /// Selected declaration sources.
    #[must_use]
    pub const fn declarations(self) -> &'input [PackageSource<'input>] {
        self.declarations
    }

    /// Conditions for this observation only.
    #[must_use]
    pub const fn export_conditions(self) -> &'input [&'input str] {
        self.export_conditions
    }
}

fn validate_modules(
    input: &NamespaceInput<'_>,
    modules: &[NamespaceModule<'_>],
) -> Result<(), RiftError> {
    input.validate_count(modules.len())?;
    let files = input
        .files
        .iter()
        .map(|source| (source.path, source))
        .collect::<BTreeMap<_, _>>();
    let mut references = 0_usize;
    let mut conditions = 0_usize;
    let mut bytes = input
        .files
        .iter()
        .chain(input.context_sources)
        .try_fold(0_u64, |bytes, source| {
            counted_bytes(bytes, source.text.len(), input.bytes_max)
        })?;
    for module in modules {
        bytes = counted_bytes(bytes, module.module.len(), input.bytes_max)?;
        if module.module.len() > SYMBOL_ID_BYTES_MAX {
            return Err(invalid());
        }
        SymbolIdentity::new(
            input.owner.clone(),
            input.language.clone(),
            vec![module.module.to_owned()],
        )
        .map_err(|_| invalid())?;
        references = references
            .checked_add(1)
            .and_then(|count| count.checked_add(module.declarations.len()))
            .ok_or_else(invalid)?;
        conditions = conditions
            .checked_add(module.export_conditions.len())
            .ok_or_else(invalid)?;
        input.validate_count(references)?;
        input.validate_count(conditions)?;
        let mut selected = BTreeSet::new();
        for source in std::iter::once(&module.implementation).chain(module.declarations) {
            let admitted = files.get(source.path).ok_or_else(invalid)?;
            if !selected.insert(source.path)
                || source.text != admitted.text
                || source.physical != admitted.physical
            {
                return Err(invalid());
            }
        }
        let mut selected_conditions = BTreeSet::new();
        for condition in module.export_conditions {
            if condition.is_empty() || !selected_conditions.insert(condition) {
                return Err(invalid());
            }
            bytes = counted_bytes(bytes, condition.len(), input.bytes_max)?;
        }
    }
    Ok(())
}

fn counted_bytes(bytes: u64, length: usize, bytes_max: u64) -> Result<u64, RiftError> {
    let observed = bytes
        .checked_add(u64::try_from(length).unwrap_or(u64::MAX))
        .ok_or_else(|| {
            errors::analysis::package_input_too_many_bytes()
                .field("package_bytes_max")
                .bound(bytes_max)
                .observed(u64::MAX)
                .error()
        })?;
    if observed > bytes_max {
        return errors::analysis::package_input_too_many_bytes()
            .field("package_bytes_max")
            .bound(bytes_max)
            .observed(observed)
            .fail();
    }
    Ok(observed)
}

fn invalid() -> RiftError {
    errors::analysis::package_input_identity_invalid().error()
}

#[cfg(test)]
mod tests;
