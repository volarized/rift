use std::fmt;

use unicode_normalization::UnicodeNormalization;

use crate::constants::{
    PROJECT_PATH_BYTES_MAX, RIFT_STATE_DIRECTORY, RIFT_STATE_DIRECTORY_PREFIX,
    SOURCE_PATH_BYTES_MAX,
};
use crate::{Error, ErrorCode, ErrorContext, ErrorName, Fault, fault_label};
use serde::Serialize;

/// Path vocabulary being validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    /// Workspace-relative filesystem path.
    Project,
    /// Location-relative source-catalog path.
    Source,
}

/// Reason a path is outside its domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathViolation {
    /// Source paths cannot be empty.
    Empty,
    /// Path exceeds its UTF-8 byte limit.
    TooLong,
    /// Absolute paths cannot cross either boundary.
    Absolute,
    /// Dot segments make ownership ambiguous.
    DotSegment,
    /// Project paths cannot contain empty segments.
    EmptySegment,
    /// Backslashes are never canonical separators.
    Backslash,
    /// Control characters cannot name source.
    ControlCharacter,
    /// Project paths must use Unicode NFC.
    NonCanonicalUnicode,
    /// Project paths cannot address Rift state.
    RiftState,
}

/// One rejected path: which vocabulary refused it, and which rule it broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathFault {
    kind: PathKind,
    violation: PathViolation,
}

impl PathFault {
    /// Returns path vocabulary that rejected input.
    #[must_use]
    pub const fn kind(self) -> PathKind {
        self.kind
    }

    /// Returns violated path rule.
    #[must_use]
    pub const fn violation(self) -> PathViolation {
        self.violation
    }
}

impl Fault for PathFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::UnsupportedPath)
    }

    fn context(&self) -> Vec<ErrorContext> {
        vec![
            ErrorContext::new("path_kind", fault_label(&self.kind)),
            ErrorContext::new("violation", fault_label(&self.violation)),
        ]
    }
}

/// Invalid project or source path.
pub type PathError = Error<PathFault>;

/// Validated path below a workspace root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectPath(String);

impl ProjectPath {
    /// Validates one project-relative path.
    ///
    /// Empty input names workspace root.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] for non-canonical or unsafe filesystem paths.
    pub fn new(value: impl Into<String>) -> Result<Self, PathError> {
        let value = value.into();
        validate_common(&value, PathKind::Project, PROJECT_PATH_BYTES_MAX)?;
        if value.chars().nfc().ne(value.chars()) {
            return Err(path_error(
                PathKind::Project,
                PathViolation::NonCanonicalUnicode,
            ));
        }
        if value == RIFT_STATE_DIRECTORY || value.starts_with(RIFT_STATE_DIRECTORY_PREFIX) {
            return Err(path_error(PathKind::Project, PathViolation::RiftState));
        }
        if value.split('/').any(str::is_empty) && !value.is_empty() {
            return Err(path_error(PathKind::Project, PathViolation::EmptySegment));
        }
        Ok(Self(value))
    }

    /// Returns canonical project-relative text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProjectPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Validated path relative to one source location.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourcePath(String);

impl SourcePath {
    /// Validates one catalog-relative source path.
    ///
    /// # Errors
    ///
    /// Returns [`PathError`] for empty, absolute, ambiguous, or oversized paths.
    pub fn new(value: impl Into<String>) -> Result<Self, PathError> {
        let value = value.into();
        validate_common(&value, PathKind::Source, SOURCE_PATH_BYTES_MAX)?;
        if value.is_empty() {
            return Err(path_error(PathKind::Source, PathViolation::Empty));
        }
        Ok(Self(value))
    }

    /// Returns location-relative path text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourcePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

fn validate_common(value: &str, kind: PathKind, bytes_max: usize) -> Result<(), PathError> {
    match path_violation(value, bytes_max) {
        Some(violation) => Err(path_error(kind, violation)),
        None => Ok(()),
    }
}

/// Classifies one path value against the rules every path kind shares. Arms
/// are ordered by precedence: the first matching rule names the violation.
fn path_violation(value: &str, bytes_max: usize) -> Option<PathViolation> {
    match value.as_bytes() {
        bytes if bytes.len() > bytes_max => Some(PathViolation::TooLong),
        [b'/', ..] => Some(PathViolation::Absolute),
        [drive, b':', ..] if drive.is_ascii_alphabetic() => Some(PathViolation::Absolute),
        bytes if bytes.contains(&b'\\') => Some(PathViolation::Backslash),
        _ if value.chars().any(char::is_control) => Some(PathViolation::ControlCharacter),
        _ if value.split('/').any(is_dot_segment) => Some(PathViolation::DotSegment),
        _ => None,
    }
}

// Explicit match per review request; equivalent `matches!` trips clippy needlessly.
#[allow(clippy::match_like_matches_macro)]
fn is_dot_segment(segment: &str) -> bool {
    match segment {
        "." | ".." => true,
        _ => false,
    }
}

fn path_error(kind: PathKind, violation: PathViolation) -> PathError {
    Error::new(PathFault { kind, violation })
}

#[cfg(test)]
mod tests {
    use super::{PathKind, PathViolation, ProjectPath, SourcePath};

    #[test]
    fn project_path_accepts_root_and_canonical_unicode() {
        assert_eq!(
            ProjectPath::new("").map(|path| path.to_string()),
            Ok(String::new())
        );
        assert_eq!(
            ProjectPath::new("src/caf\u{e9}.rs").map(|path| path.to_string()),
            Ok(String::from("src/caf\u{e9}.rs"))
        );
    }

    #[test]
    fn project_path_rejects_every_filesystem_boundary() {
        let cases = [
            ("/src/lib.rs", PathViolation::Absolute),
            ("C:/src/lib.rs", PathViolation::Absolute),
            ("C:src/lib.rs", PathViolation::Absolute),
            ("src/../lib.rs", PathViolation::DotSegment),
            ("src//lib.rs", PathViolation::EmptySegment),
            ("src\\lib.rs", PathViolation::Backslash),
            ("src/line\n.rs", PathViolation::ControlCharacter),
            ("src/cafe\u{301}.rs", PathViolation::NonCanonicalUnicode),
            (".rift/index.db", PathViolation::RiftState),
        ];

        for (value, violation) in cases {
            let error = ProjectPath::new(value).expect_err("fixture must be rejected");
            assert_eq!(error.fault().kind(), PathKind::Project);
            assert_eq!(error.fault().violation(), violation);
        }
    }

    #[test]
    fn project_path_counts_utf8_bytes() {
        assert!(ProjectPath::new("a".repeat(1_000)).is_ok());
        let value = "\u{e9}".repeat(501);
        assert_eq!(
            ProjectPath::new(value).map_err(|error| error.fault().violation()),
            Err(PathViolation::TooLong)
        );
    }

    #[test]
    fn source_path_preserves_non_project_catalog_names() {
        assert_eq!(
            SourcePath::new("serde/1.0.197/src//lib.rs").map(|path| path.to_string()),
            Ok(String::from("serde/1.0.197/src//lib.rs"))
        );
        assert_eq!(
            SourcePath::new("").map_err(|error| error.fault().violation()),
            Err(PathViolation::Empty)
        );
        assert_eq!(
            SourcePath::new("../outside.rs").map_err(|error| error.fault().violation()),
            Err(PathViolation::DotSegment)
        );
        assert!(SourcePath::new("a".repeat(4_096)).is_ok());
        assert!(SourcePath::new("é".repeat(2_048)).is_ok());
        assert_eq!(
            SourcePath::new("a".repeat(4_097)).map_err(|error| error.fault().violation()),
            Err(PathViolation::TooLong)
        );
        assert_eq!(
            SourcePath::new("é".repeat(2_049)).map_err(|error| error.fault().violation()),
            Err(PathViolation::TooLong)
        );
        for value in [
            "/absolute.rs",
            "C:/absolute.rs",
            "C:relative.rs",
            "dir\\file.rs",
            "dir/line\n.rs",
        ] {
            assert!(SourcePath::new(value).is_err(), "value={value:?}");
        }
    }

    #[test]
    fn path_error_uses_registry_explanation() {
        let error = ProjectPath::new("../outside").expect_err("dot segment is invalid");
        assert_eq!(
            error.to_string(),
            "the path cannot be addressed by this workspace: \
             path_kind project, violation dot_segment; \
             use a workspace-relative path with `/` separators and no `.` or `..` components"
        );
    }

    #[test]
    fn path_violation_labels_are_non_empty_lowercase() {
        let violations = [
            PathViolation::Empty,
            PathViolation::TooLong,
            PathViolation::Absolute,
            PathViolation::DotSegment,
            PathViolation::EmptySegment,
            PathViolation::Backslash,
            PathViolation::ControlCharacter,
            PathViolation::NonCanonicalUnicode,
            PathViolation::RiftState,
        ];
        for violation in violations {
            let label = crate::fault_label(&violation);
            assert!(!label.is_empty(), "violation={violation:?}");
            assert!(
                label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "label must be the serde snake_case name so the rendered line \
                 and the wire spelling cannot drift: violation={violation:?}, label={label}"
            );
        }
    }

    #[test]
    fn path_kind_labels_name_each_vocabulary() {
        assert_eq!(crate::fault_label(&PathKind::Project), "project");
        assert_eq!(crate::fault_label(&PathKind::Source), "source");
    }

    #[test]
    fn path_error_display_covers_project_and_source_kinds() {
        let project_error = ProjectPath::new("../outside").expect_err("dot segment is invalid");
        assert_eq!(
            project_error.to_string(),
            "the path cannot be addressed by this workspace: \
             path_kind project, violation dot_segment; \
             use a workspace-relative path with `/` separators and no `.` or `..` components"
        );

        let source_error = SourcePath::new("").expect_err("empty source path is invalid");
        assert_eq!(
            source_error.to_string(),
            "the path cannot be addressed by this workspace: \
             path_kind source, violation empty; \
             use a workspace-relative path with `/` separators and no `.` or `..` components"
        );
    }
}
