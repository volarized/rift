use std::borrow::Borrow;
use std::fmt;
use std::sync::Arc;

use unicode_normalization::UnicodeNormalization;

use crate::constants::{
    PROJECT_PATH_BYTES_MAX, RIFT_STATE_DIRECTORY, RIFT_STATE_DIRECTORY_PREFIX,
    SOURCE_PATH_BYTES_MAX,
};
use rift_error::{RiftError, errors};
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

/// Invalid project or source path.

/// Validated path below a workspace root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectPath(Arc<str>);

impl ProjectPath {
    /// Validates one project-relative path.
    ///
    /// Empty input names workspace root.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for non-canonical or unsafe filesystem paths.
    pub fn new(value: impl Into<String>) -> Result<Self, RiftError> {
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
        Ok(Self(Arc::from(value)))
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

impl AsRef<std::path::Path> for ProjectPath {
    fn as_ref(&self) -> &std::path::Path {
        std::path::Path::new(self.as_str())
    }
}

/// Lets an ordered map keyed by project path be probed by its text: the files below one
/// directory are the keys at or after its spelling with a trailing separator. Ordering,
/// equality, and hashing all derive from that same text, as the borrow requires.
impl Borrow<str> for ProjectPath {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

/// Validated path relative to one source location.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourcePath(Arc<str>);

impl SourcePath {
    /// Validates one catalog-relative source path.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for empty, absolute, ambiguous, or oversized paths.
    pub fn new(value: impl Into<String>) -> Result<Self, RiftError> {
        let value = value.into();
        validate_common(&value, PathKind::Source, SOURCE_PATH_BYTES_MAX)?;
        if value.is_empty() {
            return Err(path_error(PathKind::Source, PathViolation::Empty));
        }
        Ok(Self(Arc::from(value)))
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

fn validate_common(value: &str, kind: PathKind, bytes_max: usize) -> Result<(), RiftError> {
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

fn path_error(kind: PathKind, violation: PathViolation) -> RiftError {
    let path_kind = match kind {
        PathKind::Project => "project",
        PathKind::Source => "source",
    };
    macro_rules! build {
        ($builder:expr) => {
            $builder.path_kind(path_kind).error()
        };
    }
    match violation {
        PathViolation::Empty => build!(errors::core::path_empty()),
        PathViolation::TooLong => build!(errors::core::path_too_long()),
        PathViolation::Absolute => build!(errors::core::path_absolute()),
        PathViolation::DotSegment => build!(errors::core::path_dot_segment()),
        PathViolation::EmptySegment => build!(errors::core::path_empty_segment()),
        PathViolation::Backslash => build!(errors::core::path_backslash()),
        PathViolation::ControlCharacter => build!(errors::core::path_control_character()),
        PathViolation::NonCanonicalUnicode => {
            build!(errors::core::path_non_canonical_unicode())
        }
        PathViolation::RiftState => build!(errors::core::path_rift_state()),
    }
}

#[cfg(test)]
mod tests {
    use std::hash::{Hash as _, Hasher as _};

    use super::{PathKind, PathViolation, ProjectPath, SourcePath};

    fn path_slug(kind: PathKind, violation: PathViolation) -> &'static str {
        let kind = match kind {
            PathKind::Project => "project",
            PathKind::Source => "source",
        };
        let violation = match violation {
            PathViolation::Empty => "empty",
            PathViolation::TooLong => "too_long",
            PathViolation::Absolute => "absolute",
            PathViolation::DotSegment => "dot_segment",
            PathViolation::EmptySegment => "empty_segment",
            PathViolation::Backslash => "backslash",
            PathViolation::ControlCharacter => "control_character",
            PathViolation::NonCanonicalUnicode => "non_canonical_unicode",
            PathViolation::RiftState => "rift_state",
        };
        match violation {
            "empty" => "rift.core.path_empty",
            "too_long" => "rift.core.path_too_long",
            "absolute" => "rift.core.path_absolute",
            "dot_segment" => "rift.core.path_dot_segment",
            "empty_segment" => "rift.core.path_empty_segment",
            "backslash" => "rift.core.path_backslash",
            "control_character" => "rift.core.path_control_character",
            "non_canonical_unicode" => "rift.core.path_non_canonical_unicode",
            "rift_state" => "rift.core.path_rift_state",
            _ => unreachable!("path kind {kind} and violation {violation}"),
        }
    }

    #[test]
    fn cloned_paths_share_text_and_keep_value_semantics() {
        let project = ProjectPath::new("src/lib.rs").expect("valid project path");
        let project_clone = project.clone();
        assert!(std::sync::Arc::ptr_eq(&project.0, &project_clone.0));
        assert_eq!(project, project_clone);
        assert!(project < ProjectPath::new("src/main.rs").expect("valid project path"));
        assert_eq!(project.to_string(), "src/lib.rs");
        assert_eq!(AsRef::<std::path::Path>::as_ref(&project), std::path::Path::new("src/lib.rs"));
        assert!(ProjectPath::new("src/../lib.rs").is_err());

        let mut project_hash = std::collections::hash_map::DefaultHasher::new();
        project.hash(&mut project_hash);
        let mut clone_hash = std::collections::hash_map::DefaultHasher::new();
        project_clone.hash(&mut clone_hash);
        assert_eq!(project_hash.finish(), clone_hash.finish());

        let source = SourcePath::new("serde/src/lib.rs").expect("valid source path");
        let source_clone = source.clone();
        assert!(std::sync::Arc::ptr_eq(&source.0, &source_clone.0));
        assert_eq!(source, source_clone);
        assert_eq!(source.to_string(), "serde/src/lib.rs");
        assert!(SourcePath::new("").is_err());
    }

    #[test]
    fn project_path_accepts_root_and_canonical_unicode() {
        assert_eq!(
            ProjectPath::new("")
                .expect("workspace root is valid")
                .to_string(),
            String::new()
        );
        assert_eq!(
            ProjectPath::new("src/caf\u{e9}.rs")
                .expect("canonical unicode path is valid")
                .to_string(),
            String::from("src/caf\u{e9}.rs")
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
            assert_eq!(
                error.slug().as_str(),
                path_slug(PathKind::Project, violation)
            );
        }
    }

    #[test]
    fn project_path_counts_utf8_bytes() {
        assert!(ProjectPath::new("a".repeat(1_000)).is_ok());
        let value = "\u{e9}".repeat(501);
        assert_eq!(
            ProjectPath::new(value).map_err(|error| error.slug().as_str()),
            Err("rift.core.path_too_long")
        );
    }

    #[test]
    fn source_path_preserves_non_project_catalog_names() {
        assert_eq!(
            SourcePath::new("serde/1.0.197/src//lib.rs")
                .expect("source catalog path allows repeated separators")
                .to_string(),
            String::from("serde/1.0.197/src//lib.rs")
        );
        assert_eq!(
            SourcePath::new("").map_err(|error| error.slug().as_str()),
            Err("rift.core.path_empty")
        );
        assert_eq!(
            SourcePath::new("../outside.rs").map_err(|error| error.slug().as_str()),
            Err("rift.core.path_dot_segment")
        );
        assert!(SourcePath::new("a".repeat(4_096)).is_ok());
        assert!(SourcePath::new("é".repeat(2_048)).is_ok());
        assert_eq!(
            SourcePath::new("a".repeat(4_097)).map_err(|error| error.slug().as_str()),
            Err("rift.core.path_too_long")
        );
        assert_eq!(
            SourcePath::new("é".repeat(2_049)).map_err(|error| error.slug().as_str()),
            Err("rift.core.path_too_long")
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
            "project path contains a dot segment; \
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
            "project path contains a dot segment; \
             use a workspace-relative path with `/` separators and no `.` or `..` components"
        );

        let source_error = SourcePath::new("").expect_err("empty source path is invalid");
        assert_eq!(
            source_error.to_string(),
            "source path is empty; \
             use a workspace-relative path with `/` separators and no `.` or `..` components"
        );
    }
}
