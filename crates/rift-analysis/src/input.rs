//! Exact-package input shared by local and cloud adapters.

use std::collections::BTreeSet;

use rift_core::{
    ContributionOrigin, Error, ErrorCode, ErrorContext, ErrorName, Fault, LimitEvidence,
    ProjectPath, SourceKind, SourceLocation, SourceUnitId, fault_label,
};
use rift_protocol::index::PACKAGE_UNITS_MAX;
use rift_protocol::read::{Language, PackageIdentity};
#[cfg(feature = "collector")]
use rift_syntax::SyntaxLimits;
use serde::Serialize;

/// Bounds accepted for one exact-package input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExactPackageLimits {
    files_max: u32,
    bytes_max: u64,
    #[cfg(feature = "collector")]
    syntax: Option<SyntaxLimits>,
}

impl ExactPackageLimits {
    /// Constructs package input bounds; every source parses under its provider's declared
    /// syntax bounds.
    #[must_use]
    pub const fn new(files_max: u32, bytes_max: u64) -> Self {
        Self {
            files_max,
            bytes_max,
            #[cfg(feature = "collector")]
            syntax: None,
        }
    }

    /// Parses every source under `syntax` in place of each provider's declared bounds.
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

    /// Syntax bounds replacing each provider's declared bounds, when the caller set them.
    #[cfg(feature = "collector")]
    #[must_use]
    pub const fn syntax(self) -> Option<SyntaxLimits> {
        self.syntax
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
    /// Returns [`PackageInputError`] when identity, origin, paths, source count, or source
    /// bytes violate these rules.
    pub fn new(
        package: &'input PackageIdentity,
        language: &'input Language,
        origin: &'input ContributionOrigin,
        files: &'input [PackageSource<'input>],
        limits: ExactPackageLimits,
    ) -> Result<Self, PackageInputError> {
        validate_origin(package, origin)?;
        validate_package_identity(package)?;
        let files_bound = limits.files_max.min(PACKAGE_UNITS_MAX);
        let observed_files = u32::try_from(files.len()).unwrap_or(u32::MAX);
        if observed_files > files_bound {
            return Err(PackageInputFault::new(PackageInputViolation::TooManyFiles)
                .breached(
                    "package_files_max",
                    u64::from(files_bound),
                    u64::from(observed_files),
                )
                .into());
        }
        let mut paths = BTreeSet::new();
        let mut bytes = 0_u64;
        for file in files {
            if !paths.insert(file.path) {
                return Err(PackageInputFault::new(PackageInputViolation::DuplicatePath)
                    .at(file.path)
                    .into());
            }
            SourceUnitId::for_package(package, file.path).map_err(|error| {
                PackageInputFault::new(PackageInputViolation::InvalidIdentity)
                    .at(file.path)
                    .caused_by(error)
            })?;
            let file_bytes = u64::try_from(file.text.len()).unwrap_or(u64::MAX);
            bytes = bytes.checked_add(file_bytes).ok_or_else(|| {
                PackageInputFault::new(PackageInputViolation::TooManyBytes).breached(
                    "package_bytes_max",
                    limits.bytes_max,
                    u64::MAX,
                )
            })?;
            if bytes > limits.bytes_max {
                return Err(PackageInputFault::new(PackageInputViolation::TooManyBytes)
                    .breached("package_bytes_max", limits.bytes_max, bytes)
                    .into());
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

/// Package input violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageInputViolation {
    /// Package manager, name, or version cannot form a package source unit.
    InvalidIdentity,
    /// Origin does not identify this dependency or standard-library package.
    InvalidOrigin,
    /// Source path appears more than once.
    DuplicatePath,
    /// Source count exceeds input or publication bound.
    TooManyFiles,
    /// Aggregate source bytes exceed input bound.
    TooManyBytes,
}

/// Package input failure with path or bound evidence.
#[derive(Debug)]
pub struct PackageInputFault {
    violation: PackageInputViolation,
    path: Option<String>,
    breach: Option<(&'static str, u64, u64)>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl PackageInputFault {
    fn new(violation: PackageInputViolation) -> Self {
        Self {
            violation,
            path: None,
            breach: None,
            source: None,
        }
    }

    fn caused_by(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    fn at(mut self, path: &ProjectPath) -> Self {
        self.path = Some(path.as_str().to_owned());
        self
    }

    fn breached(mut self, field: &'static str, bound: u64, observed: u64) -> Self {
        self.breach = Some((field, bound, observed));
        self
    }

    /// Package input violation.
    #[must_use]
    pub const fn violation(&self) -> PackageInputViolation {
        self.violation
    }

    /// Package-relative path, when one caused the failure.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }
}

impl Fault for PackageInputFault {
    fn name(&self) -> ErrorName {
        let code = match self.violation {
            PackageInputViolation::TooManyFiles | PackageInputViolation::TooManyBytes => {
                ErrorCode::LimitExceeded
            }
            PackageInputViolation::InvalidIdentity
            | PackageInputViolation::InvalidOrigin
            | PackageInputViolation::DuplicatePath => ErrorCode::InvalidRequest,
        };
        ErrorName::Wire(code)
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![ErrorContext::new("violation", fault_label(&self.violation))];
        if let Some(path) = &self.path {
            context.push(ErrorContext::new("path", path.clone()));
        }
        if let Some((field, bound, observed)) = self.breach {
            context.push(ErrorContext::new("bound", format!("{field}={bound}")));
            context.push(ErrorContext::new("observed", observed.to_string()));
        }
        context
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }

    fn limit_evidence(&self) -> Option<LimitEvidence> {
        self.breach.map(|(field, limit, required)| LimitEvidence {
            field: field.to_owned(),
            limit,
            required,
        })
    }
}

/// Opaque package input failure.
pub type PackageInputError = Error<PackageInputFault>;

fn validate_package_identity(package: &PackageIdentity) -> Result<(), PackageInputError> {
    let path = ProjectPath::new("package")
        .map_err(|_| PackageInputFault::new(PackageInputViolation::InvalidIdentity))?;
    SourceUnitId::for_package(package, &path)
        .map(|_| ())
        .map_err(|error| {
            PackageInputFault::new(PackageInputViolation::InvalidIdentity)
                .caused_by(error)
                .into()
        })
}

fn validate_origin(
    package: &PackageIdentity,
    origin: &ContributionOrigin,
) -> Result<(), PackageInputError> {
    let location_matches = match origin.location() {
        Some(SourceLocation::Dependency { package: owner }) => owner == package,
        Some(SourceLocation::Stdlib {}) => true,
        Some(SourceLocation::Project { .. } | SourceLocation::External {}) | None => false,
    };
    if location_matches && origin.source_kind() == SourceKind::Authored {
        return Ok(());
    }
    Err(PackageInputFault::new(PackageInputViolation::InvalidOrigin).into())
}

#[cfg(test)]
mod tests {
    use rift_core::{ContributionOrigin, Fault, ProjectPath, SourceKind, SourceLocation};
    use rift_protocol::read::{Language, PackageIdentity};

    use super::{ExactPackageInput, ExactPackageLimits, PackageInputViolation, PackageSource};

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
            error.fault().violation(),
            PackageInputViolation::TooManyFiles
        );
        assert_eq!(error.fault().limit_evidence().expect("bound").limit, 0);
        assert_eq!(
            error.fault().limit_evidence().expect("observed").required,
            1
        );
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
            error.fault().violation(),
            PackageInputViolation::TooManyBytes
        );
        assert_eq!(error.fault().limit_evidence().expect("bound").limit, 4);
        assert_eq!(
            error.fault().limit_evidence().expect("observed").required,
            12
        );
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
            error.fault().violation(),
            PackageInputViolation::DuplicatePath
        );
        assert_eq!(error.fault().path(), Some("src/lib.rs"));
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
            error.fault().violation(),
            PackageInputViolation::InvalidOrigin
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
            error.fault().violation(),
            PackageInputViolation::InvalidOrigin
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
            error.fault().violation(),
            PackageInputViolation::InvalidIdentity
        );
    }
}
