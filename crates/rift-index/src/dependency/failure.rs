//! One dependency indexing failure: its violation, the package, and the evidence.

use std::path::{Path, PathBuf};

use rift_core::{Error, ErrorCode, ErrorContext, ErrorName, Fault, LimitEvidence, fault_label};
use rift_protocol::read::PackageIdentity;
use serde::Serialize;

use crate::workspace::WorkspaceIndexError;

/// Stable dependency indexing failure classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PackageIndexViolation {
    /// The catalog holds no source root for the package on this machine.
    SourceRootMissing,
    /// No API file selection exists for the package's language.
    LanguageUnsupported,
    /// The source root, a directory below it, or a selected file could not be read.
    Unreadable,
    /// A directory sits deeper below the source root than `directory_depth_max`.
    DirectoryDepthExceeded,
    /// The walk examined more directory entries than `walk_entries_max`.
    WalkEntriesExceeded,
    /// The package selects more files than `package_files_max`.
    PackageFilesExceeded,
    /// The package's selected files hold more bytes than `package_bytes_max`.
    PackageBytesExceeded,
    /// Every indexed package together would exceed `total_bytes_max`.
    TotalBytesExceeded,
    /// A package-relative path is not a valid project path.
    InvalidPath,
    /// The package identity cannot spell a source resolver, unit, or origin.
    Identity,
    /// No shipped provider parses a selected file, or its parse failed.
    Syntax,
    /// Publication or normalization of the package's declarations failed.
    Provider,
    /// The normalized graph holds no readable symbol for a matched declaration.
    SymbolMissing,
}

/// The bound a limit violation crossed and what the package needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LimitBreach {
    field: &'static str,
    bound: u64,
    observed: u64,
}

/// One dependency indexing failure: its violation, the package, and the evidence.
///
/// The identity is boxed so the carrier stays small enough to return by value
/// on every walk and build path.
#[derive(Debug)]
pub struct PackageIndexFault {
    violation: PackageIndexViolation,
    package: Box<PackageIdentity>,
    path: Option<PathBuf>,
    breach: Option<LimitBreach>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl PackageIndexFault {
    pub(super) fn new(violation: PackageIndexViolation, package: &PackageIdentity) -> Self {
        Self {
            violation,
            package: Box::new(package.clone()),
            path: None,
            breach: None,
            source: None,
        }
    }

    pub(super) fn at(mut self, path: &Path) -> Self {
        self.path = Some(path.to_path_buf());
        self
    }

    pub(super) fn breached(mut self, field: &'static str, bound: u64, observed: u64) -> Self {
        self.breach = Some(LimitBreach {
            field,
            bound,
            observed,
        });
        self
    }

    pub(super) fn caused_by(
        mut self,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    /// Returns stable failure classification.
    #[must_use]
    pub const fn violation(&self) -> PackageIndexViolation {
        self.violation
    }

    /// The package the failure names.
    #[must_use]
    pub const fn package(&self) -> &PackageIdentity {
        &self.package
    }

    /// Returns involved filesystem or package-relative path when available.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

impl Fault for PackageIndexFault {
    /// A syntax failure delegates to the underlying workspace index error's
    /// identity when the source downcasts to one.
    fn name(&self) -> ErrorName {
        match self.violation {
            PackageIndexViolation::SourceRootMissing => {
                ErrorName::Wire(ErrorCode::ResourceNotFound)
            }
            PackageIndexViolation::LanguageUnsupported => {
                ErrorName::Wire(ErrorCode::CapabilityUnavailable)
            }
            PackageIndexViolation::Unreadable => ErrorName::Wire(ErrorCode::StorageFailure),
            PackageIndexViolation::DirectoryDepthExceeded
            | PackageIndexViolation::WalkEntriesExceeded
            | PackageIndexViolation::PackageFilesExceeded
            | PackageIndexViolation::PackageBytesExceeded
            | PackageIndexViolation::TotalBytesExceeded => {
                ErrorName::Wire(ErrorCode::LimitExceeded)
            }
            PackageIndexViolation::InvalidPath => ErrorName::Wire(ErrorCode::UnsupportedPath),
            PackageIndexViolation::Syntax => self
                .source
                .as_deref()
                .and_then(|source| source.downcast_ref::<WorkspaceIndexError>())
                .map_or_else(
                    || ErrorName::Wire(ErrorCode::InternalError),
                    WorkspaceIndexError::name,
                ),
            PackageIndexViolation::Identity
            | PackageIndexViolation::Provider
            | PackageIndexViolation::SymbolMissing => ErrorName::Wire(ErrorCode::InternalError),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        let mut context = vec![
            ErrorContext::new("violation", fault_label(&self.violation)),
            ErrorContext::new("package", package_segment(&self.package)),
        ];
        if let Some(path) = &self.path {
            context.push(ErrorContext::new("path", path.display().to_string()));
        }
        if let Some(breach) = self.breach {
            context.push(ErrorContext::new(
                "bound",
                format!("{}={}", breach.field, breach.bound),
            ));
            context.push(ErrorContext::new("observed", breach.observed.to_string()));
        }
        context
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }

    fn limit_evidence(&self) -> Option<LimitEvidence> {
        self.breach.map(|breach| LimitEvidence {
            field: breach.field.to_owned(),
            limit: breach.bound,
            required: breach.observed,
        })
    }
}

/// Opaque dependency indexing failure.
pub type PackageIndexError = Error<PackageIndexFault>;

/// The package as an identity path segment and error label: `<manager>/<name>@<version>`.
pub(super) fn package_segment(identity: &PackageIdentity) -> String {
    format!(
        "{}/{}@{}",
        identity.manager, identity.name, identity.version
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use rift_core::{ErrorCode, ErrorName, Fault as _};

    use super::super::fixture::tokio;
    use super::{PackageIndexError, PackageIndexFault, PackageIndexViolation};

    #[test]
    fn test_package_index_error_display_names_the_package_for_every_violation() {
        let violations = [
            PackageIndexViolation::SourceRootMissing,
            PackageIndexViolation::LanguageUnsupported,
            PackageIndexViolation::Unreadable,
            PackageIndexViolation::DirectoryDepthExceeded,
            PackageIndexViolation::WalkEntriesExceeded,
            PackageIndexViolation::PackageFilesExceeded,
            PackageIndexViolation::PackageBytesExceeded,
            PackageIndexViolation::TotalBytesExceeded,
            PackageIndexViolation::InvalidPath,
            PackageIndexViolation::Identity,
            PackageIndexViolation::Syntax,
            PackageIndexViolation::Provider,
            PackageIndexViolation::SymbolMissing,
        ];
        for violation in violations {
            let error = PackageIndexError::new(
                PackageIndexFault::new(violation, &tokio()).at(Path::new("src/lib.rs")),
            );
            let rendered = error.to_string();
            assert!(
                rendered.contains("cargo/tokio@1.53.1"),
                "{violation:?} names the package: {rendered}"
            );
            assert!(
                rendered.contains("src/lib.rs"),
                "{violation:?} names the path: {rendered}"
            );
            assert!(
                error.fault().limit_evidence().is_none(),
                "{violation:?} carries no limit evidence without a breach"
            );
            assert!(std::error::Error::source(&error).is_none());
        }
        let names: Vec<ErrorName> = violations
            .iter()
            .map(|violation| PackageIndexFault::new(*violation, &tokio()).name())
            .collect();
        assert_eq!(names[0], ErrorName::Wire(ErrorCode::ResourceNotFound));
        assert_eq!(names[2], ErrorName::Wire(ErrorCode::StorageFailure));
        assert!(
            names[3..8]
                .iter()
                .all(|name| *name == ErrorName::Wire(ErrorCode::LimitExceeded))
        );
        assert_eq!(names[8], ErrorName::Wire(ErrorCode::UnsupportedPath));
        assert!(
            names[9..]
                .iter()
                .all(|name| *name == ErrorName::Wire(ErrorCode::InternalError))
        );
    }

    #[test]
    fn test_package_index_limit_breach_renders_bound_and_observed() {
        let error =
            PackageIndexError::new(
                PackageIndexFault::new(PackageIndexViolation::PackageBytesExceeded, &tokio())
                    .breached("package_bytes_max", 10, 42),
            );

        let rendered = error.to_string();

        assert!(
            rendered.contains("bound package_bytes_max=10"),
            "{rendered}"
        );
        assert!(rendered.contains("observed 42"), "{rendered}");
        let evidence = error.fault().limit_evidence().expect("limit evidence");
        assert_eq!(evidence.field, "package_bytes_max");
        assert_eq!((evidence.limit, evidence.required), (10, 42));
    }

    #[test]
    fn test_package_index_syntax_violation_delegates_to_the_workspace_error_identity() {
        let workspace_error = crate::workspace::relative_path(Path::new("../escape.rs"))
            .expect_err("a dot segment is refused");
        let expected = workspace_error.name();
        let error = PackageIndexError::new(
            PackageIndexFault::new(PackageIndexViolation::Syntax, &tokio())
                .caused_by(workspace_error),
        );

        assert_eq!(error.name(), expected);
        assert!(std::error::Error::source(&error).is_some());
    }
}
