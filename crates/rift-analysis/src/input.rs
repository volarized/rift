//! Exact-package input shared by local and cloud adapters.

use std::collections::BTreeSet;

use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation, SourceUnitId};
use rift_error::{RiftError, errors};
#[cfg(feature = "collector")]
use rift_protocol::configuration::WorkspaceConfiguration;
use rift_protocol::index::PACKAGE_SOURCE_BYTES_CEILING;
use rift_protocol::read::{Language, PackageIdentity};
#[cfg(feature = "collector")]
use rift_provider::{
    CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT, PROVIDERS_MAX_DEFAULT, PublicationLimits,
};
#[cfg(feature = "collector")]
use rift_syntax::SyntaxLimits;

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

/// One exact package and the selected source bytes it carries.
#[derive(Debug, Clone, Copy)]
pub struct ExactPackageInput<'input> {
    package: &'input PackageIdentity,
    language: &'input Language,
    origin: &'input ContributionOrigin,
    files: &'input [PackageSource<'input>],
    limits: ExactPackageLimits,
}

impl<'input> ExactPackageInput<'input> {
    /// Validates one package identity, origin, and bounded source set.
    ///
    /// Paths must be unique. The source count and aggregate bytes must fit input limits,
    /// and every path must form a source unit under the package identity.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when identity, origin, paths, source count, or source
    /// bytes violate these rules.
    pub fn new(
        package: &'input PackageIdentity,
        language: &'input Language,
        origin: &'input ContributionOrigin,
        files: &'input [PackageSource<'input>],
        limits: ExactPackageLimits,
    ) -> Result<Self, RiftError> {
        validate_origin(package, origin)?;
        validate_package_identity(package)?;
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
            SourceUnitId::for_package(package, file.path).map_err(|error| {
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
            package,
            language,
            origin,
            files,
            limits,
        })
    }

    /// Exact package identity.
    #[must_use]
    pub const fn package(self) -> &'input PackageIdentity {
        self.package
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

    /// Bounds validated for this input.
    #[must_use]
    pub const fn limits(self) -> ExactPackageLimits {
        self.limits
    }
}

fn validate_package_identity(package: &PackageIdentity) -> Result<(), RiftError> {
    let path = ProjectPath::new("package")
        .map_err(|_| errors::analysis::package_input_identity_invalid().error())?;
    SourceUnitId::for_package(package, &path)
        .map(|_| ())
        .map_err(|source| {
            errors::analysis::package_input_identity_invalid()
                .cause(source)
                .error()
        })
}

fn validate_origin(
    package: &PackageIdentity,
    origin: &ContributionOrigin,
) -> Result<(), RiftError> {
    let location_matches = match origin.location() {
        Some(SourceLocation::Dependency { package: owner }) => owner == package,
        Some(SourceLocation::Stdlib {}) => true,
        Some(SourceLocation::Project { .. } | SourceLocation::External {}) | None => false,
    };
    if location_matches && origin.source_kind() == SourceKind::Authored {
        return Ok(());
    }
    errors::analysis::package_input_origin_invalid().fail()
}

#[cfg(test)]
mod tests {
    use rift_core::{ContributionOrigin, ProjectPath, SourceKind, SourceLocation};
    use rift_protocol::read::{Language, PackageIdentity};

    use super::{ExactPackageInput, ExactPackageLimits, PackageSource};

    fn identity() -> PackageIdentity {
        PackageIdentity {
            manager: "cargo".to_owned(),
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
        let language = language();
        let origin = origin(&package);
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let sources = [PackageSource::new(&path, "fn open() {}")];

        let error = ExactPackageInput::new(
            &package,
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
        let language = language();
        let origin = origin(&package);
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let sources = [PackageSource::new(&path, "fn open() {}")];

        let error = ExactPackageInput::new(
            &package,
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
        let language = language();
        let origin = origin(&package);
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let sources = [
            PackageSource::new(&path, "fn open() {}"),
            PackageSource::new(&path, "fn close() {}"),
        ];

        let error = ExactPackageInput::new(
            &package,
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
            &package,
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
        let language = language();
        let path = ProjectPath::new("src/lib.rs").expect("path");
        let files = [PackageSource::new(&path, "source")];

        let invalid_origin = ContributionOrigin::new(
            Some(SourceLocation::Project { package: None }),
            SourceKind::Authored,
        )
        .expect("project origin");
        let error = ExactPackageInput::new(
            &package,
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
            name: "p".repeat(3_500),
            version: "1.0.0".to_owned(),
        };
        let long_path = ProjectPath::new("x".repeat(1_000)).expect("bounded project path");
        let long_files = [PackageSource::new(&long_path, "source")];
        let long_origin = origin(&long_package);
        let error = ExactPackageInput::new(
            &long_package,
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
