//! Shared project-relative glob matching: one compiled matcher, reused by workspace-visibility
//! loading (the `[source]` table) and by search's `paths` selector, so both apply identical
//! glob semantics - `*` never crosses `/`, `**` does, character classes work the same way.

use std::path::{Path, PathBuf};

use ignore::Match;
use ignore::overrides::{Override, OverrideBuilder};

use crate::workspace::{WorkspaceIndexError, WorkspaceIndexViolation, index_error_caused_by};

/// What the `[source]` globs say about one path, in the table's own precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathVerdict {
    /// `exclude` matched, so the path is dropped whatever else also matched it.
    Excluded,
    /// `force_include` matched, so the path stays visible although `.gitignore` hides it.
    ForceIncluded,
    /// Nothing dropped the path, and `include` left it standing.
    Included,
    /// A configured `include` matched nothing on this path.
    NotIncluded,
}

/// The directories a `force_include` glob can reach, owned so a walk filter can carry them.
#[derive(Debug, Clone)]
pub struct ForceIncludeReach {
    root: PathBuf,
    prefixes: Vec<PathBuf>,
}

impl ForceIncludeReach {
    /// Whether no `force_include` pattern is configured, so no walk has to run for one.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prefixes.is_empty()
    }

    /// Whether one directory can hold a path a `force_include` pattern matches. A prefix
    /// the directory is still above answers true, so the walk descends toward it.
    #[must_use]
    pub fn reaches(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        self.prefixes.iter().any(|prefix| {
            prefix.as_os_str().is_empty()
                || relative.starts_with(prefix)
                || prefix.starts_with(relative)
        })
    }
}

/// Compiled include/exclude/force-include glob matcher over paths below one root.
#[derive(Debug)]
pub struct PathMatcher {
    root: PathBuf,
    include: Option<Override>,
    include_prefixes: Vec<PathBuf>,
    exclude: Option<Override>,
    excluded_subtree_prefixes: Vec<PathBuf>,
    force_include: Option<Override>,
    force_include_reach: ForceIncludeReach,
}

impl PathMatcher {
    /// Compiles `include` and `exclude` glob lists rooted at `root`, reaching past
    /// `.gitignore` nowhere. [`Self::build_with_force_include`] compiles the reaching list too.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] when a pattern is not a valid glob.
    pub fn build(
        root: &Path,
        include: &[String],
        exclude: &[String],
    ) -> Result<Self, WorkspaceIndexError> {
        Self::build_with_force_include(root, include, exclude, &[])
    }

    /// Compiles all three glob lists rooted at `root`. Empty `include` includes every path;
    /// `exclude` drops a path whatever else matched it; `force_include` keeps a path without
    /// an `include` match, and [`PathVerdict`] states the order the three decide in.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] when a pattern is not a valid glob.
    pub fn build_with_force_include(
        root: &Path,
        include: &[String],
        exclude: &[String],
        force_include: &[String],
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
            force_include: compiled_override(root, force_include)?,
            force_include_reach: ForceIncludeReach {
                root: root.to_path_buf(),
                prefixes: force_include
                    .iter()
                    .map(|pattern| literal_prefix(pattern))
                    .collect(),
            },
        })
    }

    /// The directories this matcher's `force_include` patterns can reach, owned so a walk
    /// filter can carry them.
    #[must_use]
    pub fn force_include_reach(&self) -> ForceIncludeReach {
        self.force_include_reach.clone()
    }

    /// Whether one directory can hold a path a `force_include` pattern matches.
    #[must_use]
    pub fn force_include_reaches(&self, path: &Path) -> bool {
        self.force_include_reach.reaches(path)
    }

    /// What the three glob lists say about `path`, in the order they decide in.
    #[must_use]
    pub fn verdict(&self, path: &Path) -> PathVerdict {
        let dropped = self
            .exclude
            .as_ref()
            .is_some_and(|overrides| matches(overrides, path));
        if dropped {
            return PathVerdict::Excluded;
        }
        let forced = self
            .force_include
            .as_ref()
            .is_some_and(|overrides| matches(overrides, path));
        if forced {
            return PathVerdict::ForceIncluded;
        }
        match &self.include {
            Some(overrides) if !matches(overrides, path) => PathVerdict::NotIncluded,
            _ => PathVerdict::Included,
        }
    }

    /// Whether `path` passes the `[source]` globs alone, with `.gitignore` not consulted:
    /// [`PathVerdict::Included`] or [`PathVerdict::ForceIncluded`].
    #[must_use]
    pub fn includes(&self, path: &Path) -> bool {
        matches!(
            self.verdict(path),
            PathVerdict::Included | PathVerdict::ForceIncluded
        )
    }

    /// Whether one directory can contain a path this matcher includes.
    #[must_use]
    pub fn may_include_descendant(&self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        let reaches = |prefixes: &[PathBuf]| {
            prefixes.iter().any(|prefix| {
                prefix.as_os_str().is_empty()
                    || relative.starts_with(prefix)
                    || prefix.starts_with(relative)
            })
        };
        let included = self.include_prefixes.is_empty()
            || reaches(&self.include_prefixes)
            || self.force_include_reach.reaches(path);
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
    fn test_verdict_follows_the_source_table_precedence() {
        let root = Path::new("/workspace");
        let matcher = PathMatcher::build_with_force_include(
            root,
            &["src/**".to_owned()],
            &["notes/secret/**".to_owned()],
            &["notes/**".to_owned()],
        )
        .expect("valid globs");

        assert_eq!(
            matcher.verdict(Path::new("/workspace/notes/secret/key.txt")),
            PathVerdict::Excluded,
            "exclude decides before force_include"
        );
        assert_eq!(
            matcher.verdict(Path::new("/workspace/notes/plan.txt")),
            PathVerdict::ForceIncluded,
            "force_include keeps a path include never named"
        );
        assert_eq!(
            matcher.verdict(Path::new("/workspace/src/lib.rs")),
            PathVerdict::Included
        );
        assert_eq!(
            matcher.verdict(Path::new("/workspace/other.rs")),
            PathVerdict::NotIncluded
        );
    }

    #[test]
    fn test_includes_keeps_a_force_included_path() {
        let root = Path::new("/workspace");
        let matcher = PathMatcher::build_with_force_include(
            root,
            &["src/**".to_owned()],
            &[],
            &["notes/**".to_owned()],
        )
        .expect("valid globs");
        assert!(matcher.includes(Path::new("/workspace/notes/plan.txt")));
        assert!(matcher.may_include_descendant(Path::new("/workspace/notes")));
    }

    #[test]
    fn test_force_include_reach_covers_its_prefixes_alone() {
        let root = Path::new("/workspace");
        let matcher =
            PathMatcher::build_with_force_include(root, &[], &[], &["notes/**".to_owned()])
                .expect("valid glob");
        let reach = matcher.force_include_reach();

        assert!(!reach.is_empty());
        assert!(reach.reaches(root), "the walk starts at the root");
        assert!(reach.reaches(Path::new("/workspace/notes")));
        assert!(reach.reaches(Path::new("/workspace/notes/deep/plan.txt")));
        assert!(!reach.reaches(Path::new("/workspace/node_modules")));
        assert!(!reach.reaches(Path::new("/elsewhere/notes")));
    }

    #[test]
    fn test_force_include_reach_is_empty_without_patterns() {
        let root = Path::new("/workspace");
        let matcher = PathMatcher::build(root, &[], &[]).expect("valid globs");
        assert!(matcher.force_include_reach().is_empty());
    }

    #[test]
    fn test_invalid_force_include_glob_refuses_with_source_pattern_invalid() {
        let root = Path::new("/workspace");
        let error = PathMatcher::build_with_force_include(root, &[], &[], &["[".to_owned()])
            .expect_err("an unclosed character class must be refused");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::SourcePatternInvalid
        );
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
