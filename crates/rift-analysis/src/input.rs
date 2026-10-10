//! Exact-package input shared by local and cloud adapters.

use std::collections::BTreeSet;

use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation, SourceUnitId};
use rift_error::{RiftError, errors};
#[cfg(feature = "collector")]
use rift_protocol::configuration::WorkspaceConfiguration;
use rift_protocol::identity::{SourceDigest, SymbolOwner};
use rift_protocol::index::PACKAGE_SOURCE_BYTES_CEILING;
use rift_protocol::index::PackageArtifact;
use rift_protocol::read::Language;
#[cfg(feature = "collector")]
use rift_provider::{
    CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT, PROVIDERS_MAX_DEFAULT, PublicationLimits,
};
#[cfg(feature = "collector")]
use rift_syntax::SyntaxLimits;
use sha2::{Digest as _, Sha256};

/// Original entry kind retained by a captured source inventory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveMemberKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic or hard link, whose target is not followed.
    Link,
}

mod import_root;
pub(crate) use import_root::validate_roots;
#[cfg(feature = "collector")]
mod namespace;

#[cfg(feature = "collector")]
pub use namespace::{NamespaceInput, NamespaceModule};

pub use import_root::{
    PACKAGE_IMPORT_BYTES_MAX, PACKAGE_IMPORT_ENTRIES_MAX, PackageImportRoot,
    PackageImportRootOrigin,
};

/// Checked retained-source bounds supplied by the package caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RetainedSourceLimits {
    pub(crate) record_bytes_max: u32,
    pub(crate) total_bytes_max: u64,
}

/// Bounds accepted for one exact-package input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExactPackageLimits {
    files_max: u32,
    bytes_max: u64,
    retained_source: Option<RetainedSourceLimits>,
    publication: Option<rift_protocol::configuration::PackageConfiguration>,
    relationships_max: Option<usize>,
    documentation: Option<crate::documentation::DocumentationLimits>,
    #[cfg(feature = "collector")]
    declarations_max: Option<u32>,
    #[cfg(feature = "collector")]
    syntax: Option<SyntaxLimits>,
}

impl ExactPackageLimits {
    /// Constructs package input bounds; every source parses under the default syntax bounds.
    #[must_use]
    pub const fn new(files_max: u32, bytes_max: u64) -> Self {
        Self {
            files_max,
            bytes_max,
            retained_source: None,
            publication: None,
            relationships_max: None,
            documentation: None,
            #[cfg(feature = "collector")]
            declarations_max: None,
            #[cfg(feature = "collector")]
            syntax: None,
        }
    }

    /// Retains up to `record_bytes_max` source bytes in each publication record.
    ///
    /// `total_bytes_max` counts source strings in units, symbols and search documents,
    /// including their copies. The analyzer refuses before allocating a string that
    /// passes that total. Without this override, records keep the default source bound.
    ///
    /// # Errors
    ///
    /// Returns a registered error for zero bounds, a total below the record bound, or
    /// a record bound above the publication's supported source ceiling.
    pub fn with_retained_source_bytes(
        self,
        record_bytes_max: u32,
        total_bytes_max: u64,
    ) -> Result<Self, RiftError> {
        let record_accepted =
            record_bytes_max > 0 && record_bytes_max <= PACKAGE_SOURCE_BYTES_CEILING;
        let total_accepted = total_bytes_max >= u64::from(record_bytes_max);
        if !record_accepted || !total_accepted {
            return errors::analysis::package_retained_source_limits_invalid()
                .record_bytes_max(u64::from(record_bytes_max))
                .total_bytes_max(total_bytes_max)
                .record_bytes_ceiling(u64::from(PACKAGE_SOURCE_BYTES_CEILING))
                .fail();
        }
        Ok(Self {
            retained_source: Some(RetainedSourceLimits {
                record_bytes_max,
                total_bytes_max,
            }),
            ..self
        })
    }

    #[cfg(feature = "collector")]
    pub(crate) const fn retained_source(self) -> Option<RetainedSourceLimits> {
        self.retained_source
    }

    /// Parses every source under `syntax` in place of the default bounds.
    ///
    /// A caller that must analyze packages carrying one outsized generated file raises the
    /// bounds here; the package source and byte bounds still apply.
    #[cfg(feature = "collector")]
    #[must_use]
    pub const fn with_syntax(self, syntax: SyntaxLimits) -> Self {
        Self {
            syntax: Some(syntax),
            ..self
        }
    }

    /// Applies a positive declaration bound to semantic assembly.
    ///
    /// Package publication bounds still apply after assembly and module joining.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `declarations_max` is zero.
    #[cfg(feature = "collector")]
    pub fn with_declarations(self, declarations_max: u32) -> Result<Self, RiftError> {
        let maximum = declarations_max as usize;
        PublicationLimits::new(PROVIDERS_MAX_DEFAULT, maximum, maximum)?;
        Ok(Self {
            declarations_max: Some(declarations_max),
            ..self
        })
    }

    /// Maximum parsed declarations admitted to semantic assembly, or the provider's default bound.
    #[cfg(feature = "collector")]
    #[must_use]
    pub fn declarations_max(self) -> usize {
        self.declarations_max
            .map_or(CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT, |maximum| {
                maximum as usize
            })
    }

    /// Uses the `[source]` and `[providers.syntax]` bounds for one package analysis.
    ///
    /// Callers may accept it with `rift_core::acceptance::accept_configuration`, which
    /// applies `RIFT_SOURCE_*` and `RIFT_PROVIDERS_SYNTAX_*` environment overrides.
    /// This method validates the configuration before constructing the analysis bounds.
    /// Source selection remains the caller's responsibility; package publication bounds
    /// still apply to the assembled records.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the configuration violates its advertised bounds.
    #[cfg(feature = "collector")]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "configuration validation bounds files and declarations below u32::MAX"
    )]
    pub fn from_configuration(configuration: &WorkspaceConfiguration) -> Result<Self, RiftError> {
        configuration
            .validate()
            .map_err(|violation| rift_core::configuration_violation_error(&violation))?;
        let source = &configuration.source;
        let syntax = SyntaxLimits::from_configuration(&configuration.providers.syntax)?;
        let documentation = crate::documentation::DocumentationLimits::from_configuration(
            &configuration.documentation,
        )?;
        Self::new(source.files as u32, source.workspace_size.bytes())
            .with_syntax(syntax)
            .with_documentation(documentation)
            .with_declarations(source.declarations as u32)?
            .with_relationships(source.relationships as usize)?
            .with_publication(configuration.package)
    }

    /// Applies validated `[package]` publication counts and retained-source bounds.
    /// Archive admission uses `archive::ArchiveLimits::from_configuration` separately.
    ///
    /// # Errors
    /// Returns a registered configuration error for an unsupported or unordered bound.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "validation bounds retained source below u32::MAX"
    )]
    pub fn with_publication(
        self,
        configuration: rift_protocol::configuration::PackageConfiguration,
    ) -> Result<Self, RiftError> {
        configuration
            .validate()
            .map_err(|violation| rift_core::configuration_violation_error(&violation))?;
        let limits = self.with_retained_source_bytes(
            configuration.retained_source.bytes() as u32,
            configuration
                .retained_total
                .map_or(u64::MAX, rift_protocol::configuration::ByteSize::bytes),
        )?;
        Ok(Self {
            publication: Some(configuration),
            ..limits
        })
    }

    pub(crate) fn publication(self) -> rift_protocol::configuration::PackageConfiguration {
        self.publication.unwrap_or_default()
    }

    /// Sets the relationship edge capacity used by semantic assembly.
    ///
    /// # Errors
    /// Returns a configuration error for zero or capacity above the supported range.
    pub fn with_relationships(self, maximum: usize) -> Result<Self, RiftError> {
        let source = rift_protocol::source::SourceConfiguration {
            relationships: maximum as u64,
            ..rift_protocol::source::SourceConfiguration::default()
        };
        let configuration = rift_protocol::configuration::WorkspaceConfiguration {
            source,
            ..rift_protocol::configuration::WorkspaceConfiguration::default()
        };
        configuration
            .validate()
            .map_err(|violation| rift_core::configuration_violation_error(&violation))?;
        Ok(Self {
            relationships_max: Some(maximum),
            ..self
        })
    }

    #[cfg(feature = "collector")]
    pub(crate) fn relationships_max(self) -> usize {
        self.relationships_max.unwrap_or_else(|| {
            usize::try_from(rift_protocol::source::SOURCE_RELATIONSHIPS_DEFAULT)
                .expect("default relationship count fits usize")
        })
    }

    /// Applies accepted documentation collection bounds.
    #[must_use]
    pub fn with_documentation(self, limits: crate::documentation::DocumentationLimits) -> Self {
        Self {
            documentation: Some(limits),
            ..self
        }
    }

    #[cfg(feature = "collector")]
    pub(crate) fn documentation(self) -> crate::documentation::DocumentationLimits {
        self.documentation.unwrap_or_default()
    }

    /// Syntax bounds every source parses under: the caller's, or the default bounds.
    #[cfg(feature = "collector")]
    #[must_use]
    pub fn syntax(self) -> SyntaxLimits {
        self.syntax.unwrap_or_default()
    }

    /// Maximum source count.
    #[must_use]
    pub const fn files_max(self) -> u32 {
        self.files_max
    }

    /// Maximum aggregate source bytes.
    #[must_use]
    pub const fn bytes_max(self) -> u64 {
        self.bytes_max
    }
}

/// One package-relative UTF-8 source file.
#[derive(Debug, Clone, Copy)]
pub struct PackageSource<'source> {
    path: &'source ProjectPath,
    text: &'source str,
    physical: Option<(&'source SourceUnitId, &'source ContributionOrigin)>,
    source_digest: Option<rift_core::FileDigest>,
}

impl<'source> PackageSource<'source> {
    /// Names one package-relative UTF-8 source file.
    #[must_use]
    pub const fn new(path: &'source ProjectPath, text: &'source str) -> Self {
        Self {
            path,
            text,
            physical: None,
            source_digest: None,
        }
    }

    /// Retains a released physical source unit and its origin beside the analysis path.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the unit has no released owner or its origin differs.
    pub fn with_source_unit(
        mut self,
        unit: &'source SourceUnitId,
        origin: &'source ContributionOrigin,
    ) -> Result<Self, RiftError> {
        let owner = unit
            .source_owner()
            .ok_or_else(|| errors::analysis::package_input_origin_invalid().error())?;
        validate_owner(owner)?;
        validate_physical_origin(owner, origin)?;
        self.physical = Some((unit, origin));
        Ok(self)
    }

    /// Retains a captured external source with its original path and complete digest.
    ///
    /// Acquisition and captured-view membership are validated by the caller.
    ///
    /// # Errors
    /// Returns [`RiftError`] when the unit, origin, original path, or source digest differs.
    pub fn with_captured_source_unit(
        mut self,
        unit: &'source SourceUnitId,
        origin: &'source ContributionOrigin,
        original_path: &rift_core::SourcePath,
        digest: &SourceDigest,
    ) -> Result<Self, RiftError> {
        validate_captured_origin(unit, origin)?;
        let actual = Sha256::digest(self.text.as_bytes());
        if unit.key() != original_path || format!("{actual:x}") != digest.as_str() {
            return errors::analysis::package_input_identity_invalid()
                .path(self.path.as_str())
                .fail();
        }
        self.physical = Some((unit, origin));
        self.source_digest = Some(rift_core::FileDigest::from_bytes(actual.into()));
        Ok(self)
    }

    pub(crate) fn source_digest(self) -> rift_core::FileDigest {
        self.source_digest
            .unwrap_or_else(|| rift_core::FileDigest::of(self.text.as_bytes()))
    }

    /// Explicit physical source unit, when supplied.
    #[must_use]
    pub const fn source_unit(self) -> Option<&'source SourceUnitId> {
        match self.physical {
            Some((unit, _)) => Some(unit),
            None => None,
        }
    }

    /// Origin of the explicit physical source unit, when supplied.
    #[must_use]
    pub const fn origin(self) -> Option<&'source ContributionOrigin> {
        match self.physical {
            Some((_, origin)) => Some(origin),
            None => None,
        }
    }

    /// Package-relative path.
    #[must_use]
    pub const fn path(self) -> &'source ProjectPath {
        self.path
    }

    /// Complete UTF-8 source.
    #[must_use]
    pub const fn text(self) -> &'source str {
        self.text
    }
}

/// One exact package or runtime owner and its selected source bytes.
#[derive(Debug, Clone, Copy)]
pub struct ExactPackageInput<'input> {
    owner: &'input SymbolOwner,
    language: &'input Language,
    origin: &'input ContributionOrigin,
    files: &'input [PackageSource<'input>],
    context_sources: &'input [PackageSource<'input>],
    frameworks: &'input [rift_protocol::configuration::SyntaxFrameworkConfiguration],
    import_roots: &'input [PackageImportRoot],
    artifact: Option<&'input PackageArtifact>,
    #[cfg(feature = "collector")]
    modules: &'input [NamespaceModule<'input>],
    #[cfg(feature = "collector")]
    build_paths: Option<
        &'input std::collections::BTreeMap<
            rift_protocol::read::ProjectPath,
            Option<ArchiveMemberKind>,
        >,
    >,
    limits: ExactPackageLimits,
}

impl<'input> ExactPackageInput<'input> {
    /// Validates one exact package or runtime owner, origin, and bounded source set.
    ///
    /// Paths must be unique. The source count and aggregate bytes must fit input limits,
    /// and every path must form a source unit under that owner.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when identity, origin, paths, source count, or source
    /// bytes violate these rules.
    pub fn new(
        owner: &'input SymbolOwner,
        language: &'input Language,
        origin: &'input ContributionOrigin,
        files: &'input [PackageSource<'input>],
        limits: ExactPackageLimits,
    ) -> Result<Self, RiftError> {
        validate_owner(owner)?;
        validate_origin(owner, origin)?;
        validate_sources(files, limits, |file| released_source_unit(owner, file))?;
        Ok(Self {
            owner,
            language,
            origin,
            files,
            context_sources: &[],
            frameworks: &[],
            import_roots: &[],
            artifact: None,
            #[cfg(feature = "collector")]
            modules: &[],
            #[cfg(feature = "collector")]
            build_paths: None,
            limits,
        })
    }

    /// Exact defining package, runtime, or compiler owner.
    #[must_use]
    pub const fn owner(self) -> &'input SymbolOwner {
        self.owner
    }

    /// Borrows the admitted source and metadata context for namespace placement.
    #[cfg(feature = "collector")]
    #[must_use]
    pub fn namespace_input(self) -> NamespaceInput<'input> {
        NamespaceInput::from_package(&self)
    }

    /// Adds selected module observations validated against this source inventory.
    ///
    /// # Errors
    /// Returns the existing input refusal for invalid witnesses or exceeded bounds.
    #[cfg(feature = "collector")]
    pub fn with_modules(
        mut self,
        modules: &'input [NamespaceModule<'input>],
    ) -> Result<Self, RiftError> {
        self.namespace_input().with_modules(modules)?;
        self.modules = modules;
        Ok(self)
    }

    /// Adds captured build-path observations under the source count and byte bounds.
    ///
    /// An absent key is unknown. `None` records a verified missing path.
    /// Archive observations require the complete admitted member inventory.
    ///
    /// # Errors
    /// Returns the existing input refusal for invalid paths or exceeded bounds.
    #[cfg(feature = "collector")]
    pub fn with_build_paths(
        mut self,
        paths: &'input std::collections::BTreeMap<
            rift_protocol::read::ProjectPath,
            Option<ArchiveMemberKind>,
        >,
    ) -> Result<Self, RiftError> {
        self.namespace_input().with_build_paths(paths)?;
        self.build_paths = Some(paths);
        Ok(self)
    }

    /// Adds established import roots without rewriting original source paths.
    ///
    /// # Errors
    ///
    /// Returns an identity error for duplicate prefixes or exceeded aggregate bounds.
    pub fn with_import_roots(
        mut self,
        roots: &'input [PackageImportRoot],
    ) -> Result<Self, RiftError> {
        import_root::validate_roots(roots)?;
        self.import_roots = roots;
        Ok(self)
    }

    /// Adds the validated selected artifact and its full content digest.
    #[must_use]
    pub const fn with_artifact(mut self, artifact: &'input PackageArtifact) -> Self {
        self.artifact = Some(artifact);
        self
    }

    /// Established import roots; an empty set leaves import placement unresolved.
    #[must_use]
    pub const fn import_roots(self) -> &'input [PackageImportRoot] {
        self.import_roots
    }

    /// Selected artifact, when its immutable content was captured.
    #[must_use]
    pub const fn artifact(self) -> Option<&'input PackageArtifact> {
        self.artifact
    }

    /// Catalog language.
    #[must_use]
    pub const fn language(self) -> &'input Language {
        self.language
    }

    /// Source origin.
    #[must_use]
    pub const fn origin(self) -> &'input ContributionOrigin {
        self.origin
    }

    /// Selected source files.
    #[must_use]
    pub const fn files(self) -> &'input [PackageSource<'input>] {
        self.files
    }

    /// Adds bounded package metadata and explicit framework selections.
    ///
    /// Selected files already belong to context. Additional sources must have distinct
    /// paths and share the same aggregate source count and byte bounds.
    ///
    /// # Errors
    ///
    /// Returns the same source validation failures as [`Self::new`].
    pub fn with_framework_context(
        mut self,
        sources: &'input [PackageSource<'input>],
        frameworks: &'input [rift_protocol::configuration::SyntaxFrameworkConfiguration],
    ) -> Result<Self, RiftError> {
        let observed = self.files.len().saturating_add(sources.len());
        let bound = self.limits.files_max.min(self.limits.publication().units);
        if observed > bound as usize {
            return errors::analysis::package_input_too_many_files()
                .field("package_files_max")
                .bound(u64::from(bound))
                .observed(u64::try_from(observed).unwrap_or(u64::MAX))
                .fail();
        }
        let combined = self
            .files
            .iter()
            .chain(sources)
            .copied()
            .collect::<Vec<_>>();
        ExactPackageInput::new(
            self.owner,
            self.language,
            self.origin,
            &combined,
            self.limits,
        )?;
        self.context_sources = sources;
        self.frameworks = frameworks;
        #[cfg(feature = "collector")]
        if let Some(paths) = self.build_paths {
            self.namespace_input().with_build_paths(paths)?;
        }
        Ok(self)
    }

    /// Additional captured package context sources.
    #[must_use]
    pub const fn context_sources(self) -> &'input [PackageSource<'input>] {
        self.context_sources
    }

    /// Explicit source framework selections.
    #[must_use]
    pub const fn frameworks(
        self,
    ) -> &'input [rift_protocol::configuration::SyntaxFrameworkConfiguration] {
        self.frameworks
    }

    /// Bounds validated for this input.
    #[must_use]
    pub const fn limits(self) -> ExactPackageLimits {
        self.limits
    }
}

fn validate_origin(owner: &SymbolOwner, origin: &ContributionOrigin) -> Result<(), RiftError> {
    if origin.source_kind() != SourceKind::Authored {
        return errors::analysis::package_input_origin_invalid().fail();
    }
    validate_physical_origin(owner, origin)
}

fn validate_physical_origin(
    owner: &SymbolOwner,
    origin: &ContributionOrigin,
) -> Result<(), RiftError> {
    let observed = match origin.location() {
        Some(SourceLocation::Dependency { package }) => package.owner().ok(),
        Some(SourceLocation::Stdlib {
            runtime: Some(runtime),
        }) => runtime.owner().ok(),
        _ => None,
    };
    if observed.as_ref() == Some(owner) && origin.source_kind() != SourceKind::Synthetic {
        return Ok(());
    }
    errors::analysis::package_input_origin_invalid().fail()
}

fn validate_source_count(observed: usize, limits: ExactPackageLimits) -> Result<(), RiftError> {
    validate_source_count_at_bound(
        observed,
        limits.files_max.min(limits.publication().units) as usize,
    )
}

fn validate_source_count_at_bound(observed: usize, bound: usize) -> Result<(), RiftError> {
    if observed > bound {
        return errors::analysis::package_input_too_many_files()
            .field("package_files_max")
            .bound(u64::try_from(bound).unwrap_or(u64::MAX))
            .observed(u64::try_from(observed).unwrap_or(u64::MAX))
            .fail();
    }
    Ok(())
}

fn validate_sources(
    files: &[PackageSource<'_>],
    limits: ExactPackageLimits,
    source_unit: impl Fn(&PackageSource<'_>) -> Result<SourceUnitId, RiftError>,
) -> Result<(), RiftError> {
    validate_source_count(files.len(), limits)?;
    validate_source_bytes_and_units(files, limits.bytes_max(), source_unit)
}

fn validate_source_bytes_and_units(
    files: &[PackageSource<'_>],
    bytes_max: u64,
    source_unit: impl Fn(&PackageSource<'_>) -> Result<SourceUnitId, RiftError>,
) -> Result<(), RiftError> {
    let mut paths = BTreeSet::new();
    let mut units = BTreeSet::new();
    let mut bytes = 0_u64;
    for file in files {
        if !paths.insert(file.path) {
            return errors::analysis::package_input_duplicate_path()
                .path(file.path.as_str())
                .fail();
        }
        let unit = source_unit(file)?;
        if !units.insert(unit) {
            return errors::analysis::package_input_duplicate_path()
                .path(file.path.as_str())
                .fail();
        }
        let file_bytes = u64::try_from(file.text.len()).unwrap_or(u64::MAX);
        bytes = bytes.checked_add(file_bytes).ok_or_else(|| {
            errors::analysis::package_input_too_many_bytes()
                .field("package_bytes_max")
                .bound(bytes_max)
                .observed(u64::MAX)
                .error()
        })?;
        if bytes > bytes_max {
            return errors::analysis::package_input_too_many_bytes()
                .field("package_bytes_max")
                .bound(bytes_max)
                .observed(bytes)
                .fail();
        }
    }
    Ok(())
}

fn released_source_unit(
    owner: &SymbolOwner,
    file: &PackageSource<'_>,
) -> Result<SourceUnitId, RiftError> {
    Ok(match file.physical {
        Some((unit, physical_origin)) => {
            match unit.source_owner() {
                Some(physical_owner) => validate_physical_origin(physical_owner, physical_origin)?,
                None => validate_captured_origin(unit, physical_origin)?,
            }
            unit.clone()
        }
        None => SourceUnitId::for_owner(owner.clone(), file.path.as_str()).map_err(|error| {
            errors::analysis::package_input_identity_invalid()
                .path(file.path.as_str())
                .cause(error)
                .error()
        })?,
    })
}

fn validate_captured_origin(
    unit: &SourceUnitId,
    origin: &ContributionOrigin,
) -> Result<(), RiftError> {
    if unit.source_owner().is_none()
        && unit.resolver().as_str() == "external"
        && matches!(origin.location(), Some(SourceLocation::External {}))
        && origin.source_kind() != SourceKind::Synthetic
    {
        return Ok(());
    }
    errors::analysis::package_input_origin_invalid().fail()
}

fn validate_owner(owner: &SymbolOwner) -> Result<(), RiftError> {
    SourceUnitId::for_owner(owner.clone(), "package")
        .map(|_| ())
        .map_err(|source| {
            errors::analysis::package_input_identity_invalid()
                .cause(source)
                .error()
        })
}

#[cfg(test)]
mod tests {
    use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation, SourceUnitId};
    use rift_protocol::read::{Language, PackageIdentity};

    use super::{ExactPackageInput, ExactPackageLimits, PackageSource};
    use rift_protocol::identity::{SourceDigest, SymbolOwner};
    use rift_protocol::read::RuntimeIdentity;
    use sha2::{Digest as _, Sha256};

    #[cfg(feature = "collector")]
    #[test]
    fn released_build_paths_preserve_unknown_absence_and_member_kinds() {
        use super::ArchiveMemberKind;
        use rift_protocol::read::ProjectPath as ObservationPath;
        use std::collections::BTreeMap;

        let package = identity();
        let owner = package.owner().expect("package owner");
        let language = language();
        let origin = origin(&package);
        let path = ProjectPath::new("src/lib.rs").expect("source path");
        let files = [PackageSource::new(&path, "source")];
        let paths = BTreeMap::from([
            (
                ObservationPath(String::new()),
                Some(ArchiveMemberKind::Directory),
            ),
            (
                ObservationPath("src".to_owned()),
                Some(ArchiveMemberKind::Directory),
            ),
            (
                ObservationPath("src/lib.rs".to_owned()),
                Some(ArchiveMemberKind::File),
            ),
            (
                ObservationPath("hook.py".to_owned()),
                Some(ArchiveMemberKind::Link),
            ),
            (ObservationPath("setup.py".to_owned()), None),
        ]);
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &files,
            ExactPackageLimits::new(6, 64),
        )
        .expect("released input");
        assert!(input.namespace_input().build_paths().is_none());
        let captured = input.with_build_paths(&paths).expect("captured paths");
        let view = captured.namespace_input();
        assert_eq!(view.owner(), &owner);
        assert_eq!(view.files()[0].path(), &path);
        assert_eq!(view.build_paths(), Some(&paths));
        assert_eq!(
            view.build_paths()
                .expect("observations")
                .get(&ObservationPath("setup.py".to_owned())),
            Some(&None)
        );
        assert!(
            !view
                .build_paths()
                .expect("observations")
                .contains_key(&ObservationPath("unknown.py".to_owned()))
        );
        let invalid = BTreeMap::from([(ObservationPath("../outside".to_owned()), None)]);
        assert!(input.with_build_paths(&invalid).is_err());
        assert!(input.namespace_input().build_paths().is_none());
    }

    #[cfg(feature = "collector")]
    #[test]
    fn released_build_path_bounds_survive_framework_setter_order() {
        use rift_protocol::read::ProjectPath as ObservationPath;
        use std::collections::BTreeMap;

        let package = identity();
        let owner = package.owner().expect("package owner");
        let language = language();
        let origin = origin(&package);
        let path = ProjectPath::new("src/lib.rs").expect("source path");
        let metadata_path = ProjectPath::new("Cargo.toml").expect("metadata path");
        let files = [PackageSource::new(&path, "source")];
        let metadata = [PackageSource::new(&metadata_path, "context")];
        let paths = BTreeMap::from([(ObservationPath("src".to_owned()), None)]);
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &files,
            ExactPackageLimits::new(3, 16),
        )
        .expect("released input");
        let first = input
            .with_build_paths(&paths)
            .expect("paths first")
            .with_framework_context(&metadata, &[])
            .expect("exact aggregate bounds");
        let second = input
            .with_framework_context(&metadata, &[])
            .expect("context first")
            .with_build_paths(&paths)
            .expect("same aggregate bounds");
        assert_eq!(
            first.namespace_input().build_paths(),
            second.namespace_input().build_paths()
        );
        for limits in [
            ExactPackageLimits::new(2, 16),
            ExactPackageLimits::new(3, 15),
        ] {
            let bounded = ExactPackageInput::new(&owner, &language, &origin, &files, limits)
                .expect("sources fit before additions");
            let paths_first = bounded.with_build_paths(&paths).expect("initial paths fit");
            assert!(paths_first.with_framework_context(&metadata, &[]).is_err());
            let context_first = bounded
                .with_framework_context(&metadata, &[])
                .expect("initial context fits");
            assert!(context_first.with_build_paths(&paths).is_err());
            assert_eq!(paths_first.namespace_input().build_paths(), Some(&paths));
            assert!(context_first.namespace_input().build_paths().is_none());
        }
    }

    #[test]
    fn captured_external_source_requires_original_path_digest_and_origin() {
        let alias = ProjectPath::new("runtime/module.pyi").expect("analysis path");
        let original =
            rift_core::SourcePath::new("captured/stdlib/module.pyi").expect("original source path");
        let unit = SourceUnitId::new(
            rift_core::SourceResolverId::new("external").expect("external resolver"),
            original.clone(),
        )
        .expect("captured unit");
        let external =
            ContributionOrigin::new(Some(SourceLocation::External {}), SourceKind::Authored)
                .expect("physical origin");
        let text = "def open() -> None: ...";
        let digest = SourceDigest::parse(&format!("{:x}", Sha256::digest(text.as_bytes())))
            .expect("complete source digest");
        let source = PackageSource::new(&alias, text)
            .with_captured_source_unit(&unit, &external, &original, &digest)
            .expect("captured source proof");
        assert_eq!(source.path(), &alias);
        assert_eq!(source.source_unit(), Some(&unit));
        assert_eq!(source.origin(), Some(&external));
        assert_eq!(source.text(), text);
        let checked = rift_core::FileDigest::of(text.as_bytes());
        assert_eq!(source.source_digest, Some(checked));
        assert_eq!(source.source_digest(), checked);
        let copied = source;
        assert_eq!(copied.source_digest, Some(checked));
        let ordinary = PackageSource::new(&alias, text);
        assert_eq!(ordinary.source_digest, None);
        assert_eq!(ordinary.source_digest(), checked);
        assert!(
            PackageSource::new(&alias, text)
                .with_source_unit(&unit, &external)
                .is_err()
        );
        let wrong_path = rift_core::SourcePath::new("other/module.pyi").expect("other path");
        assert!(
            PackageSource::new(&alias, text)
                .with_captured_source_unit(&unit, &external, &wrong_path, &digest)
                .is_err()
        );
        assert!(
            PackageSource::new(&alias, "changed source")
                .with_captured_source_unit(&unit, &external, &original, &digest)
                .is_err()
        );
        let package = identity();
        let package_origin = origin(&package);
        assert!(
            PackageSource::new(&alias, text)
                .with_captured_source_unit(&unit, &package_origin, &original, &digest)
                .is_err()
        );
        let owner = package.owner().expect("logical package owner");
        let language = language();
        let syntax = crate::package_syntax::PackageSyntaxSource::new(
            source,
            &owner,
            &language,
            rift_syntax::SyntaxLimits::default(),
        );
        assert_eq!(syntax.identity().source_digest, checked);
        let files = [source];
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &package_origin,
            &files,
            ExactPackageLimits::new(1, 64),
        )
        .expect("independent logical owner");
        assert_eq!(input.owner(), &owner);
        let second_path = ProjectPath::new("runtime/second.pyi").expect("second analysis path");
        let second = PackageSource::new(&second_path, text)
            .with_captured_source_unit(&unit, &external, &original, &digest)
            .expect("same physical source");
        let files = [source, second];
        assert!(
            ExactPackageInput::new(
                &owner,
                &language,
                &package_origin,
                &files,
                ExactPackageLimits::new(2, 128),
            )
            .is_err()
        );
    }

    #[test]
    fn physical_source_override_keeps_origin_separate_and_refuses_duplicate_units() {
        let package = identity();
        let physical_owner = package.owner().expect("physical package owner");
        let physical_origin = origin(&package);
        let unit = rift_core::SourceUnitId::for_owner(physical_owner, "src/lib.rs")
            .expect("physical source unit");
        let alias = ProjectPath::new("runtime/core.rs").expect("analysis path");
        let source = PackageSource::new(&alias, "pub fn open() {}")
            .with_source_unit(&unit, &physical_origin)
            .expect("explicit physical association");
        assert_eq!(source.path(), &alias);
        assert_eq!(source.source_unit(), Some(&unit));
        assert_eq!(source.origin(), Some(&physical_origin));
        let runtime = RuntimeIdentity {
            runtime: "rust".to_owned(),
            version: "1.98.0".to_owned(),
        };
        let owner = runtime.owner().expect("logical runtime owner");
        let logical_origin = ContributionOrigin::new(
            Some(SourceLocation::Stdlib {
                runtime: Some(runtime),
            }),
            SourceKind::Authored,
        )
        .expect("logical runtime origin");
        let files = [source];
        let language = language();
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &logical_origin,
            &files,
            ExactPackageLimits::new(1, 64),
        )
        .expect("distinct logical owner");
        assert_eq!(input.owner(), &owner);
        assert!(
            PackageSource::new(&alias, "source")
                .with_source_unit(&unit, &logical_origin)
                .is_err()
        );
        let custom = rift_core::SourceUnitId::new(
            rift_core::SourceResolverId::new("external").expect("resolver"),
            rift_core::SourcePath::new("original.rs").expect("original path"),
        )
        .expect("custom source unit");
        assert!(
            PackageSource::new(&alias, "source")
                .with_source_unit(&custom, &physical_origin)
                .is_err()
        );
        let second_path = ProjectPath::new("runtime/other.rs").expect("second analysis path");
        let second = PackageSource::new(&second_path, "source")
            .with_source_unit(&unit, &physical_origin)
            .expect("same physical unit");
        let files = [source, second];
        let error = ExactPackageInput::new(
            &owner,
            &language,
            &logical_origin,
            &files,
            ExactPackageLimits::new(2, 64),
        )
        .expect_err("duplicate physical unit");
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_input_duplicate_path"
        );
    }

    #[test]
    fn exact_runtime_input_preserves_owner_without_a_package() {
        let runtime = RuntimeIdentity {
            runtime: "cpython".to_owned(),
            version: "3.14.3".to_owned(),
        };
        let owner = runtime.owner().expect("runtime owner");
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Stdlib {
                runtime: Some(runtime.clone()),
            }),
            SourceKind::Authored,
        )
        .expect("runtime origin");
        let language = Language::from_identity_segment("python").expect("Python language");
        let path = ProjectPath::new("sys/__init__.pyi").expect("source path");
        let files = [PackageSource::new(&path, "def exit() -> None: ...")];
        let input = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &files,
            ExactPackageLimits::new(1, 64),
        )
        .expect("exact runtime input");
        assert_eq!(input.owner(), &owner);
        assert_eq!(input.files()[0].path(), &path);
        let wrong_owner = SymbolOwner::Runtime {
            runtime: "cpython".to_owned(),
            version: "3.13.3".to_owned(),
        };
        let error = ExactPackageInput::new(
            &wrong_owner,
            &language,
            &origin,
            &files,
            ExactPackageLimits::new(1, 64),
        )
        .expect_err("different runtime version");
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_input_origin_invalid"
        );
    }

    #[test]
    fn exact_input_refuses_foreign_registry_local_and_unversioned_runtime_owners() {
        let package = identity();
        let origin = origin(&package);
        let language = language();
        let path = ProjectPath::new("src/lib.rs").expect("source path");
        let files = [PackageSource::new(&path, "pub fn open() {}")];
        let mut foreign = package.clone();
        foreign.registry = "registry.example/index".to_owned();
        let foreign = foreign.owner().expect("foreign registry owner");
        let error = ExactPackageInput::new(
            &foreign,
            &language,
            &origin,
            &files,
            ExactPackageLimits::new(1, 64),
        )
        .expect_err("same name and version do not establish registry equality");
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_input_origin_invalid"
        );
        for local in [
            SymbolOwner::Local,
            SymbolOwner::NamedLocal {
                name: "cloud".to_owned(),
            },
        ] {
            let error = ExactPackageInput::new(
                &local,
                &language,
                &origin,
                &files,
                ExactPackageLimits::new(1, 64),
            )
            .expect_err("released input refuses local owner");
            assert_eq!(
                error.slug().as_str(),
                "rift.analysis.package_input_identity_invalid"
            );
        }
        let runtime = rift_protocol::read::RuntimeIdentity {
            runtime: "rustc".to_owned(),
            version: "1.98.0".to_owned(),
        }
        .owner()
        .expect("runtime owner");
        let unknown = ContributionOrigin::new(
            Some(SourceLocation::Stdlib { runtime: None }),
            SourceKind::Authored,
        )
        .expect("unresolved runtime origin");
        assert!(
            ExactPackageInput::new(
                &runtime,
                &language,
                &unknown,
                &files,
                ExactPackageLimits::new(1, 64),
            )
            .is_err()
        );
    }

    fn identity() -> PackageIdentity {
        PackageIdentity {
            manager: "cargo".to_owned(),
            registry: "crates.io".to_owned(),
            name: "beacon".to_owned(),
            version: "1.0.0".to_owned(),
        }
    }

    fn origin(package: &PackageIdentity) -> ContributionOrigin {
        ContributionOrigin::new(
            Some(SourceLocation::Dependency {
                package: package.clone(),
            }),
            SourceKind::Authored,
        )
        .expect("origin")
    }

    fn language() -> Language {
        Language::from_identity_segment("rust").expect("Rust language")
    }

    #[test]
    fn source_count_bound_is_validated_before_analysis() {
        let package = identity();
        let owner = package.owner().expect("package owner");
        let language = language();
        let origin = origin(&package);
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let sources = [PackageSource::new(&path, "fn open() {}")];

        let error = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &sources,
            ExactPackageLimits::new(0, 64),
        )
        .expect_err("source count exceeds bound");

        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_input_too_many_files"
        );
        let context = error.context().collect::<Vec<_>>();
        assert!(context.contains(&("bound", "0".to_owned())));
        assert!(context.contains(&("observed", "1".to_owned())));
    }

    #[test]
    fn aggregate_source_bytes_bound_is_validated_before_analysis() {
        let package = identity();
        let owner = package.owner().expect("package owner");
        let language = language();
        let origin = origin(&package);
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let sources = [PackageSource::new(&path, "fn open() {}")];

        let error = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &sources,
            ExactPackageLimits::new(1, 4),
        )
        .expect_err("source bytes exceed bound");

        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_input_too_many_bytes"
        );
        let context = error.context().collect::<Vec<_>>();
        assert!(context.contains(&("bound", "4".to_owned())));
        assert!(context.contains(&("observed", "12".to_owned())));
    }

    #[test]
    fn duplicate_package_paths_are_rejected() {
        let package = identity();
        let owner = package.owner().expect("package owner");
        let language = language();
        let origin = origin(&package);
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let sources = [
            PackageSource::new(&path, "fn open() {}"),
            PackageSource::new(&path, "fn close() {}"),
        ];

        let error = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &sources,
            ExactPackageLimits::new(2, 64),
        )
        .expect_err("duplicate path");

        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_input_duplicate_path"
        );
        assert!(
            error
                .context()
                .any(|(key, value)| key == "path" && value == "src/lib.rs")
        );
    }

    #[test]
    fn non_authored_package_origin_is_rejected() {
        let package = identity();
        let owner = package.owner().expect("package owner");
        let language = language();
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Dependency {
                package: package.clone(),
            }),
            SourceKind::Generated,
        )
        .expect("generated dependency origin");
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let sources = [PackageSource::new(&path, "fn open() {}")];

        let error = ExactPackageInput::new(
            &owner,
            &language,
            &origin,
            &sources,
            ExactPackageLimits::new(1, 64),
        )
        .expect_err("package input requires authored dependency origin");

        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_input_origin_invalid"
        );
    }

    #[test]
    fn package_input_refuses_location_mismatch_and_source_unit_overflow() {
        let package = identity();
        let owner = package.owner().expect("package owner");
        let language = language();
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let files = [PackageSource::new(&path, "source")];

        let invalid_origin = ContributionOrigin::new(
            Some(SourceLocation::Project { package: None }),
            SourceKind::Authored,
        )
        .expect("project origin");
        let error = ExactPackageInput::new(
            &owner,
            &language,
            &invalid_origin,
            &files,
            ExactPackageLimits::new(1, 64),
        )
        .expect_err("package input refuses project origin");
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_input_origin_invalid"
        );

        let long_package = PackageIdentity {
            manager: "cargo".to_owned(),
            registry: format!("r.example/{}", "a".repeat(4_086)),
            name: "p".repeat(3_500),
            version: "1.0.0".to_owned(),
        };
        let long_path = ProjectPath::new("x".repeat(1_000)).expect("bounded project path");
        let long_files = [PackageSource::new(&long_path, "source")];
        let long_origin = origin(&long_package);
        let long_owner = long_package.owner().expect("bounded package owner");
        let error = ExactPackageInput::new(
            &long_owner,
            &language,
            &long_origin,
            &long_files,
            ExactPackageLimits::new(1, 64),
        )
        .expect_err("combined package and path exceed source path bound");
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.package_input_identity_invalid"
        );
    }
}
