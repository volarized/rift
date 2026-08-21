//! Shared project-relative glob matching: one compiled matcher, reused by workspace-visibility
//! loading (the `[source]` table) and by search's `paths` selector, so both apply identical
//! glob semantics — `*` never crosses `/`, `**` does, character classes work the same way.

use std::path::Path;

use ignore::Match;
use ignore::overrides::{Override, OverrideBuilder};

use crate::workspace::{WorkspaceIndexError, WorkspaceIndexViolation, index_error_caused_by};

/// Compiled include/exclude glob matcher over paths below one root.
#[derive(Debug)]
pub struct PathMatcher {
    include: Option<Override>,
    exclude: Option<Override>,
}

impl PathMatcher {
    /// Compiles `include` and `exclude` glob lists rooted at `root`. Empty `include` admits
    /// every path; a path matching `exclude` is dropped even where `include` also matched it.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] when a pattern is not a valid glob.
    pub fn build(
        root: &Path,
        include: &[String],
        exclude: &[String],
    ) -> Result<Self, WorkspaceIndexError> {
        Ok(Self {
            include: compiled_override(root, include)?,
            exclude: compiled_override(root, exclude)?,
        })
    }

    /// Whether `path` passes: admitted by `include` whenever configured, and not dropped by
    /// `exclude`.
    #[must_use]
    pub fn admits(&self, path: &Path) -> bool {
        let admitted = self
            .include
            .as_ref()
            .is_none_or(|overrides| matches(overrides, path));
        let dropped = self
            .exclude
            .as_ref()
            .is_some_and(|overrides| matches(overrides, path));
        admitted && !dropped
    }
}

fn compiled_override(
    root: &Path,
    patterns: &[String],
) -> Result<Option<Override>, WorkspaceIndexError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = OverrideBuilder::new(root);
    for pattern in patterns {
        builder.add(pattern).map_err(|error| {
            index_error_caused_by(WorkspaceIndexViolation::SourcePatternInvalid, None, error)
        })?;
    }
    builder.build().map(Some).map_err(|error| {
        index_error_caused_by(WorkspaceIndexViolation::SourcePatternInvalid, None, error)
    })
}

fn matches(overrides: &Override, path: &Path) -> bool {
    matches!(overrides.matched(path, false), Match::Whitelist(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_include_admits_every_path_and_exclude_drops_matches() {
        let root = Path::new("/workspace");
        let matcher =
            PathMatcher::build(root, &[], &["src/generated/**".to_owned()]).expect("valid globs");
        assert!(matcher.admits(Path::new("/workspace/src/lib.rs")));
        assert!(!matcher.admits(Path::new("/workspace/src/generated/gen.rs")));
    }

    #[test]
    fn test_include_narrows_and_star_does_not_cross_slash() {
        let root = Path::new("/workspace");
        let matcher = PathMatcher::build(root, &["src/*.rs".to_owned()], &[]).expect("valid glob");
        assert!(matcher.admits(Path::new("/workspace/src/lib.rs")));
        assert!(!matcher.admits(Path::new("/workspace/src/nested/deep.rs")));
    }

    #[test]
    fn test_double_star_crosses_slash() {
        let root = Path::new("/workspace");
        let matcher =
            PathMatcher::build(root, &["src/**/*.rs".to_owned()], &[]).expect("valid glob");
        assert!(matcher.admits(Path::new("/workspace/src/nested/deep.rs")));
    }

    #[test]
    fn test_exclude_wins_over_include_on_the_same_path() {
        let root = Path::new("/workspace");
        let matcher = PathMatcher::build(
            root,
            &["src/**".to_owned()],
            &["src/generated/**".to_owned()],
        )
        .expect("valid globs");
        assert!(matcher.admits(Path::new("/workspace/src/lib.rs")));
        assert!(!matcher.admits(Path::new("/workspace/src/generated/gen.rs")));
    }

    #[test]
    fn test_invalid_glob_refuses_with_source_pattern_invalid() {
        let root = Path::new("/workspace");
        let error = PathMatcher::build(root, &["[".to_owned()], &[])
            .expect_err("an unclosed character class must be refused");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::SourcePatternInvalid
        );
    }
}
