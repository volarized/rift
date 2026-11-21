//! Shared project-relative glob matching: one compiled matcher, reused by workspace-visibility
//! loading (the `[source]` table), by search's `paths` selector, and by package file selection,
//! so all three apply identical glob semantics - `*` never crosses `/`, `**` does, character
//! classes work the same way.

use std::path::{Path, PathBuf};

use ignore::Match;
use ignore::overrides::{Override, OverrideBuilder};
use rift_error::{RiftError, errors};

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
    excluded_subtrees: Option<Override>,
    force_include: Option<Override>,
    force_include_reach: ForceIncludeReach,
}

impl PathMatcher {
    /// Compiles `include` and `exclude` glob lists rooted at `root`, reaching past
    /// `.gitignore` nowhere. [`Self::build_with_force_include`] compiles the reaching list too.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a pattern is not a valid glob.
    pub fn build(root: &Path, include: &[String], exclude: &[String]) -> Result<Self, RiftError> {
        Self::build_with_force_include(root, include, exclude, &[])
    }

    /// Compiles all three glob lists rooted at `root`. Empty `include` includes every path;
    /// `exclude` drops a path whatever else matched it; `force_include` keeps a path without
    /// an `include` match, and [`PathVerdict`] states the order the three decide in.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when a pattern is not a valid glob.
    pub fn build_with_force_include(
        root: &Path,
        include: &[String],
        exclude: &[String],
        force_include: &[String],
    ) -> Result<Self, RiftError> {
        let compiled_include = compiled_override(root, include)?;
        let compiled_exclude = compiled_override(root, exclude)?;
        let excluded_subtrees =
            compiled_excluded_subtrees(root, exclude, compiled_exclude.as_ref());
        Ok(Self {
            root: root.to_path_buf(),
            include: compiled_include,
            include_prefixes: include
                .iter()
                .map(|pattern| literal_prefix(pattern))
                .collect(),
            exclude: compiled_exclude,
            excluded_subtrees,
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

    /// Whether `exclude` proves every file below `path` excluded.
    ///
    /// The root stays reachable. Recursive directory patterns use the same glob backend
    /// as file matching. Negated excludes and ambiguous directory patterns keep the full walk.
    #[must_use]
    pub fn excludes_descendants(&self, path: &Path) -> bool {
        let Some(overrides) = &self.excluded_subtrees else {
            return false;
        };
        path.ancestors()
            .take_while(|ancestor| {
                *ancestor != self.root.as_path() && ancestor.starts_with(&self.root)
            })
            .any(|ancestor| matches!(overrides.matched(ancestor, true), Match::Whitelist(_)))
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
        included && !self.excludes_descendants(path)
    }
}

/// A literal path, or complete directory components before a glob's first metacharacter.
fn literal_prefix(pattern: &str) -> PathBuf {
    // An unanchored pattern without a separator matches a file name at any depth.
    if !pattern.starts_with('/') && !pattern.contains('/') {
        return PathBuf::new();
    }
    let Some(pattern) = plain_root_relative_pattern(pattern) else {
        return PathBuf::new();
    };
    let Some(end) = pattern
        .char_indices()
        .find_map(|(index, character)| "*?[{".contains(character).then_some(index))
    else {
        return PathBuf::from(pattern.trim_end_matches('/'));
    };
    pattern[..end]
        .rsplit_once('/')
        .map_or_else(PathBuf::new, |(directory, _)| PathBuf::from(directory))
}

/// Compiles directory patterns that prove every descendant excluded.
fn compiled_excluded_subtrees(
    root: &Path,
    patterns: &[String],
    exclude: Option<&Override>,
) -> Option<Override> {
    if exclude.is_some_and(|overrides| overrides.num_ignores() > 0) {
        return None;
    }
    let parents: Vec<_> = patterns
        .iter()
        .filter_map(|pattern| {
            if matches!(pattern.as_str(), "**" | "/**") {
                return Some("/**".to_owned());
            }
            let parent = pattern.strip_suffix("/**")?;
            if parent.is_empty()
                || pattern.starts_with(['!', '#'])
                || parent.ends_with(['/', '\\'])
                || parent.contains("//")
                || parent.split('/').any(|part| matches!(part, "." | ".."))
            {
                return None;
            }
            // The original separator anchors a single-segment parent too. Escapes remain
            // unchanged for the same backend to compile.
            Some(if parent.starts_with('/') {
                parent.to_owned()
            } else {
                format!("/{parent}")
            })
        })
        .collect();
    // Original patterns have already passed admission. An unproved derived pattern
    // keeps the full walk without changing which files the original matcher includes.
    compiled_override(root, &parents).ok().flatten()
}

/// Normalizes simple anchored patterns; complex escaping stays conservative.
fn plain_root_relative_pattern(pattern: &str) -> Option<&str> {
    let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
    (!pattern.contains('\\') && !pattern.starts_with(['!', '#'])).then_some(pattern)
}

fn compiled_override(root: &Path, patterns: &[String]) -> Result<Option<Override>, RiftError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = OverrideBuilder::new(root);
    for pattern in patterns {
        builder.add(pattern).map_err(|source| {
            errors::analysis::source_pattern_invalid()
                .pattern(pattern.clone())
                .source(source)
                .error()
        })?;
    }
    builder.build().map(Some).map_err(|source| {
        errors::analysis::source_pattern_invalid()
            .source(source)
            .error()
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
    fn test_recursive_excludes_keep_compiled_anchors_and_escapes() {
        let root = Path::new("/workspace");
        for (pattern, directory, excluded) in [
            ("build/*", "build/nested", false),
            ("build/*.rs", "build/nested", false),
            ("build/**", "build", true),
            ("build/**", "build/nested", true),
            ("build/**", "other/build", false),
            ("/build/**", "build", true),
            ("/build/**", "other/build", false),
            ("**/build/**", "other/build", true),
            (r"build/\[cache\]/**", "build/[cache]", true),
            (r"build/\[cache\]/**", "build/c", false),
            (r"\!build/**", "!build", true),
            ("#build/**", "build", false),
            ("build//**", "build", false),
            ("**", "other", true),
        ] {
            let matcher = PathMatcher::build_with_force_include(
                root,
                &[],
                &[pattern.to_owned()],
                &["**".to_owned()],
            )
            .expect("valid glob");
            let directory = root.join(directory);
            assert_eq!(
                matcher.excludes_descendants(&directory),
                excluded,
                "{pattern}: {directory:?}"
            );
            assert_eq!(
                matcher.may_include_descendant(&directory),
                !excluded,
                "{pattern}: {directory:?}"
            );
            if excluded {
                for file in ["direct.rs", "nested/deep.rs"] {
                    assert!(
                        !matcher.includes(&directory.join(file)),
                        "{pattern}: {file}"
                    );
                }
            }
            assert!(!matcher.excludes_descendants(Path::new("/workspace-other/build")));
        }
    }

    #[test]
    fn test_negated_excludes_keep_directories_reachable() {
        let root = Path::new("/workspace");
        let matcher = PathMatcher::build_with_force_include(
            root,
            &[],
            &["build/**".to_owned(), "!build/kept.rs".to_owned()],
            &["build/**".to_owned()],
        )
        .expect("valid globs");
        assert!(!matcher.excludes_descendants(&root.join("build")));
        assert!(matcher.may_include_descendant(&root.join("build")));
        assert!(matcher.includes(&root.join("build/kept.rs")));
        assert!(!matcher.includes(&root.join("build/hidden.rs")));
    }

    #[test]
    fn test_child_directory_excludes_keep_the_root_reachable() {
        let root = Path::new("/workspace");
        let matcher = PathMatcher::build(root, &[], &["*/**".to_owned()]).expect("valid glob");
        assert!(matcher.includes(&root.join("kept.rs")));
        assert!(!matcher.excludes_descendants(root));
        assert!(matcher.may_include_descendant(root));
        assert!(matcher.excludes_descendants(&root.join("build")));
    }

    #[test]
    fn test_unanchored_file_names_reach_nested_directories() {
        let root = Path::new("/workspace");
        for (pattern, reaches) in [("kept.rs", true), ("/kept.rs", false)] {
            let matcher = PathMatcher::build(root, &[pattern.to_owned()], &[]).expect("valid glob");
            assert_eq!(
                matcher.may_include_descendant(&root.join("nested")),
                reaches,
                "{pattern}"
            );
            assert_eq!(
                matcher.includes(&root.join("nested/kept.rs")),
                reaches,
                "{pattern}"
            );
            let forced =
                PathMatcher::build_with_force_include(root, &[], &[], &[pattern.to_owned()])
                    .expect("valid force_include glob");
            assert_eq!(
                forced.force_include_reaches(&root.join("nested")),
                reaches,
                "{pattern}"
            );
        }
    }

    #[test]
    fn test_partial_glob_components_keep_matching_directories_reachable() {
        let root = Path::new("/workspace");
        for pattern in [
            "src/g*/**",
            "/src/g*/**",
            "src/{generated,other}/**",
            "src/[g]*/**",
        ] {
            let matcher = PathMatcher::build(root, &[pattern.to_owned()], &[]).expect("valid glob");
            assert!(
                matcher.includes(&root.join("src/generated/kept.rs")),
                "{pattern}"
            );
            assert!(
                matcher.may_include_descendant(&root.join("src/generated")),
                "{pattern}"
            );
            let forced =
                PathMatcher::build_with_force_include(root, &[], &[], &[pattern.to_owned()])
                    .expect("valid force_include glob");
            assert!(
                forced.force_include_reaches(&root.join("src/generated")),
                "{pattern}"
            );
        }
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
    fn test_invalid_force_include_glob_refuses_naming_the_pattern() {
        let root = Path::new("/workspace");
        let error = PathMatcher::build_with_force_include(root, &[], &[], &["[".to_owned()])
            .expect_err("an unclosed character class must be refused");
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.source_pattern_invalid"
        );
        assert!(
            error
                .context()
                .any(|(key, value)| key == "pattern" && value == "[")
        );
    }

    #[test]
    fn test_invalid_glob_refuses_naming_the_pattern_and_its_cause() {
        use std::error::Error as _;

        let root = Path::new("/workspace");
        let error = PathMatcher::build(root, &["src/**".to_owned(), "[".to_owned()], &[])
            .expect_err("an unclosed character class must be refused");
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.source_pattern_invalid"
        );
        assert!(
            error
                .context()
                .any(|(key, value)| key == "pattern" && value == "[")
        );
        assert!(
            error.source().is_some(),
            "the glob compiler's refusal is the cause"
        );
    }

    /// Wildcards one glob may carry and still parse, past what the matcher they compile
    /// to fits in: globset caps the compiled matcher at 10 MiB, and each `?` compiles to
    /// its own byte class.
    const OVERSIZED_GLOB_WILDCARDS: usize = 1_000_000;

    /// Each pattern parses, and the list refuses only when it compiles as a whole, so the
    /// refusal names no single pattern and keeps the compiler's cause.
    #[test]
    fn test_a_glob_list_past_the_compiled_size_limit_refuses_naming_no_pattern() {
        use std::error::Error as _;

        let root = Path::new("/workspace");
        let oversized = "?".repeat(OVERSIZED_GLOB_WILDCARDS);
        let error = PathMatcher::build(root, &[oversized], &[])
            .expect_err("a matcher past the compiled size limit must be refused");
        assert_eq!(
            error.slug().as_str(),
            "rift.analysis.source_pattern_invalid"
        );
        assert!(!error.context().any(|(key, _)| key == "pattern"));
        assert!(
            error.source().is_some(),
            "the glob compiler's refusal is the cause"
        );
    }
}
