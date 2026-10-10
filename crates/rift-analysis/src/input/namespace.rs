use std::collections::{BTreeMap, BTreeSet};

use rift_error::{RiftError, errors};
use rift_protocol::identity::{
    SYMBOL_ID_BYTES_MAX, SymbolIdentity, SymbolOwner, parse_local_scope,
};
use rift_protocol::read::Language;
use rift_syntax::SyntaxLimits;

use super::{ExactPackageInput, ExactPackageLimits, PackageImportRoot, PackageSource};

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
    modules: &'input [NamespaceModule<'input>],
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
            modules: input.modules,
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
        Ok(Self {
            owner,
            language,
            files,
            context_sources,
            import_roots,
            files_max,
            bytes_max,
            syntax,
            modules: &[],
        })
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
        validate_modules(self, modules)?;
        self.modules = modules;
        Ok(self)
    }

    /// Selected module observations and their separate export conditions.
    #[must_use]
    pub const fn modules(self) -> &'input [NamespaceModule<'input>] {
        self.modules
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
    input: NamespaceInput<'_>,
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
