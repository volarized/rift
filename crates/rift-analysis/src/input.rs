//! Exact-package input shared by local and cloud adapters.

use std::collections::BTreeSet;

use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation, SourceUnitId};
use rift_error::{RiftError, errors};
#[cfg(feature = "collector")]
use rift_protocol::configuration::WorkspaceConfiguration;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::index::PACKAGE_SOURCE_BYTES_CEILING;
use rift_protocol::index::PackageArtifact;
use rift_protocol::read::Language;
#[cfg(feature = "collector")]
use rift_provider::{
    CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT, PROVIDERS_MAX_DEFAULT, PublicationLimits,
};
#[cfg(feature = "collector")]
use rift_syntax::SyntaxLimits;

mod import_root;

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
}

impl<'source> PackageSource<'source> {
    /// Names one package-relative UTF-8 source file.
    #[must_use]
    pub const fn new(path: &'source ProjectPath, text: &'source str) -> Self {
        Self { path, text }
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
        let files_bound = limits.files_max.min(limits.publication().units);
        let observed_files = u32::try_from(files.len()).unwrap_or(u32::MAX);
        if observed_files > files_bound {
            return errors::analysis::package_input_too_many_files()
                .field("package_files_max")
                .bound(u64::from(files_bound))
                .observed(u64::from(observed_files))
                .fail();
        }
        let mut paths = BTreeSet::new();
        let mut bytes = 0_u64;
        for file in files {
            if !paths.insert(file.path) {
                return errors::analysis::package_input_duplicate_path()
                    .path(file.path.as_str())
                    .fail();
            }
            SourceUnitId::for_owner(owner.clone(), file.path.as_str()).map_err(|error| {
                errors::analysis::package_input_identity_invalid()
                    .path(file.path.as_str())
                    .cause(error)
                    .error()
            })?;
            let file_bytes = u64::try_from(file.text.len()).unwrap_or(u64::MAX);
            bytes = bytes.checked_add(file_bytes).ok_or_else(|| {
                errors::analysis::package_input_too_many_bytes()
                    .field("package_bytes_max")
                    .bound(limits.bytes_max)
                    .observed(u64::MAX)
                    .error()
            })?;
            if bytes > limits.bytes_max {
                return errors::analysis::package_input_too_many_bytes()
                    .field("package_bytes_max")
                    .bound(limits.bytes_max)
                    .observed(bytes)
                    .fail();
            }
        }
        Ok(Self {
            owner,
            language,
            origin,
            files,
            context_sources: &[],
            frameworks: &[],
            import_roots: &[],
            artifact: None,
            limits,
        })
    }

    /// Exact defining package, runtime, or compiler owner.
    #[must_use]
    pub const fn owner(self) -> &'input SymbolOwner {
        self.owner
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
    let observed = match origin.location() {
        Some(SourceLocation::Dependency { package }) => package.owner().ok(),
        Some(SourceLocation::Stdlib {
            runtime: Some(runtime),
        }) => runtime.owner().ok(),
        _ => None,
    };
    if observed.as_ref() == Some(owner) && origin.source_kind() == SourceKind::Authored {
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
    use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
    use rift_protocol::read::{Language, PackageIdentity};

    use super::{ExactPackageInput, ExactPackageLimits, PackageSource};
    use rift_protocol::identity::SymbolOwner;
    use rift_protocol::read::RuntimeIdentity;

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
