//! Shared project-relative glob matching: one compiled matcher, reused by workspace-visibility
//! loading (the `[source]` table) and by search's `paths` selector, so both apply identical
//! glob semantics — `*` never crosses `/`, `**` does, character classes work the same way.

use std::path::{Path, PathBuf};

use ignore::Match;
use ignore::overrides::{Override, OverrideBuilder};

use crate::workspace::{WorkspaceIndexError, WorkspaceIndexViolation, index_error_caused_by};

/// Compiled include/exclude glob matcher over paths below one root.
#[derive(Debug)]
pub struct PathMatcher {
    root: PathBuf,
    include: Option<Override>,
    include_prefixes: Vec<PathBuf>,
    exclude: Option<Override>,
    excluded_subtree_prefixes: Vec<PathBuf>,
}

impl PathMatcher {
    /// Compiles `include` and `exclude` glob lists rooted at `root`. Empty `include` includes
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
            root: root.to_path_buf(),
            include: compiled_override(root, include)?,
            include_prefixes: include
                .iter()
                .map(|pattern| literal_prefix(pattern))
                .collect(),
            exclude: compiled_override(root, exclude)?,
            excluded_subtree_prefixes: exclude
                .iter()
                .filter_map(|pattern| excluded_subtree_prefix(pattern))
                .collect(),
        })
    }

    /// Whether `path` passes: included whenever `include` is configured, and not dropped by
    /// `exclude`.
    #[must_use]
    pub fn includes(&self, path: &Path) -> bool {
        let included = match &self.include {
            Some(overrides) => matches(overrides, path),
            None => true,
        };
        let dropped = match &self.exclude {
            Some(overrides) => matches(overrides, path),
            None => false,
        };
        included && !dropped
    }

    /// Whether one directory can contain a path this matcher includes.
    #[must_use]
    pub fn may_include_descendant(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        let included = self.include_prefixes.is_empty()
            || self.include_prefixes.iter().any(|prefix| {
                prefix.as_os_str().is_empty()
                    || relative.starts_with(prefix)
                    || prefix.starts_with(relative)
            });
        let dropped = self
            .excluded_subtree_prefixes
            .iter()
            .any(|prefix| relative.starts_with(prefix));
        included && !dropped
    }
}

/// Literal directory prefix before one glob's first metacharacter.
fn literal_prefix(pattern: &str) -> PathBuf {
    let Some(pattern) = plain_root_relative_pattern(pattern) else {
        return PathBuf::new();
    };
    let end = pattern
        .char_indices()
        .find_map(|(index, character)| "*?[".contains(character).then_some(index))
        .unwrap_or(pattern.len());
    PathBuf::from(pattern[..end].trim_end_matches('/'))
}

/// Prefix of one pattern that proves every descendant excluded.
fn excluded_subtree_prefix(pattern: &str) -> Option<PathBuf> {
    let pattern = plain_root_relative_pattern(pattern)?;
    let prefix = pattern.strip_suffix("/**")?.trim_end_matches('/');
    (!prefix.chars().any(|character| "*?[".contains(character))).then(|| PathBuf::from(prefix))
}

/// Normalizes simple anchored patterns; complex escaping stays conservative.
fn plain_root_relative_pattern(pattern: &str) -> Option<&str> {
    let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
    (!pattern.contains('\\') && !pattern.starts_with(['!', '#'])).then_some(pattern)
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
    fn test_empty_include_includes_every_path_and_exclude_drops_matches() {
        let root = Path::new("/workspace");
        let matcher =
            PathMatcher::build(root, &[], &["src/generated/**".to_owned()]).expect("valid globs");
        assert!(matcher.includes(Path::new("/workspace/src/lib.rs")));
        assert!(!matcher.includes(Path::new("/workspace/src/generated/gen.rs")));
    }

    #[test]
    fn test_include_narrows_and_star_does_not_cross_slash() {
        let root = Path::new("/workspace");
        let matcher = PathMatcher::build(root, &["src/*.rs".to_owned()], &[]).expect("valid glob");
        assert!(matcher.includes(Path::new("/workspace/src/lib.rs")));
        assert!(!matcher.includes(Path::new("/workspace/src/nested/deep.rs")));
    }

    #[test]
    fn test_directory_inclusion_refuses_paths_outside_root() {
        let root = Path::new("/workspace");
        let matcher = PathMatcher::build(root, &["src/**".to_owned()], &[]).expect("valid glob");
        assert!(!matcher.may_include_descendant(Path::new("/elsewhere/src")));
    }

    #[test]
    fn test_directory_inclusion_tracks_possible_includes_and_excluded_subtrees() {
        let root = Path::new("/workspace");
        let matcher = PathMatcher::build(
            root,
            &["src/**".to_owned()],
            &["src/generated/**".to_owned()],
        )
        .expect("valid globs");
        assert!(matcher.may_include_descendant(Path::new("/workspace/src")));
        assert!(!matcher.may_include_descendant(Path::new("/workspace/examples")));
        assert!(!matcher.may_include_descendant(Path::new("/workspace/src/generated")));

        let direct_only = PathMatcher::build(
            root,
            &["src/**".to_owned()],
            &["src/generated/*.rs".to_owned()],
        )
        .expect("valid direct-child exclusion");
        assert!(direct_only.may_include_descendant(Path::new("/workspace/src/generated")));
        assert!(!direct_only.includes(Path::new("/workspace/src/generated/direct.rs")));
        assert!(direct_only.includes(Path::new("/workspace/src/generated/nested/lib.rs")));
    }

    #[test]
    fn test_directory_inclusion_normalizes_anchors_and_keeps_escapes_conservative() {
        let root = Path::new("/workspace");
        let anchored =
            PathMatcher::build(root, &["/src/**".to_owned()], &[]).expect("valid anchored glob");
        assert!(anchored.may_include_descendant(Path::new("/workspace/src")));
        assert!(!anchored.may_include_descendant(Path::new("/workspace/examples")));

        for escaped in [r"\!generated/**", r"src/\[generated\]/**"] {
            let matcher =
                PathMatcher::build(root, &[escaped.to_owned()], &[]).expect("valid escaped glob");
            assert!(matcher.may_include_descendant(Path::new("/workspace/elsewhere")));
        }
    }

    #[test]
    fn test_double_star_crosses_slash() {
        let root = Path::new("/workspace");
        let matcher =
            PathMatcher::build(root, &["src/**/*.rs".to_owned()], &[]).expect("valid glob");
        assert!(matcher.includes(Path::new("/workspace/src/nested/deep.rs")));
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
        assert!(matcher.includes(Path::new("/workspace/src/lib.rs")));
        assert!(!matcher.includes(Path::new("/workspace/src/generated/gen.rs")));
    }

    #[test]
    fn test_includes_matches_a_candidate_path_built_with_join() {
        // Patterns are always forward-slash; the candidate path is not. Building it with
        // `Path::join` instead of a forward-slash literal exercises the OS-native separator
        // `ignore::overrides::Override` sees on every platform, Windows included.
        let root = Path::new("/workspace");
        let matcher =
            PathMatcher::build(root, &["src/**/*.rs".to_owned()], &[]).expect("valid glob");
        let candidate = root.join("src").join("nested").join("deep.rs");
        assert!(matcher.includes(&candidate));
        let excluded = root.join("other.rs");
        assert!(!matcher.includes(&excluded));
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
