//! Which cataloged packages the dependency index plans: the `[dependencies]` `include`
//! and `exclude` globs, compiled once and matched against each package's
//! `<manager>/<name>`.

use std::path::Path;
use std::sync::Arc;

use rift_protocol::dependencies::DependenciesConfiguration;
use rift_protocol::read::PackageIdentity;

use crate::glob::PathMatcher;
use crate::workspace::WorkspaceIndexError;

/// The compiled `[dependencies]` `include` and `exclude` lists, matched against each
/// cataloged package's `<manager>/<name>` through the glob engine the `[source]` policy
/// uses. Two selections are equal when their pattern lists are.
#[derive(Clone, Debug)]
pub struct PackageSelection {
    include: Vec<String>,
    exclude: Vec<String>,
    matcher: Arc<PathMatcher>,
}

impl PackageSelection {
    /// Compiles the table's `include` and `exclude` lists.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceIndexError`] when a pattern is not a valid glob.
    pub fn compile(configuration: &DependenciesConfiguration) -> Result<Self, WorkspaceIndexError> {
        let include: Vec<String> = configuration
            .include
            .iter()
            .map(|pattern| pattern.0.clone())
            .collect();
        let exclude: Vec<String> = configuration
            .exclude
            .iter()
            .map(|pattern| pattern.0.clone())
            .collect();
        let matcher = PathMatcher::build(Path::new(""), &include, &exclude)?;
        Ok(Self {
            include,
            exclude,
            matcher: Arc::new(matcher),
        })
    }

    /// Whether the index plans `identity`'s package: `include` is empty or matches its
    /// `<manager>/<name>`, and `exclude` does not.
    #[must_use]
    pub fn selects(&self, identity: &PackageIdentity) -> bool {
        let name = format!("{}/{}", identity.manager, identity.name);
        self.matcher.includes(Path::new(&name))
    }

    /// The `include` patterns as the table spelled them.
    #[must_use]
    pub fn include(&self) -> &[String] {
        &self.include
    }

    /// The `exclude` patterns as the table spelled them.
    #[must_use]
    pub fn exclude(&self) -> &[String] {
        &self.exclude
    }
}

impl Default for PackageSelection {
    /// Every cataloged package: the default table's empty lists.
    fn default() -> Self {
        Self::compile(&DependenciesConfiguration::default())
            .unwrap_or_else(|error| unreachable!("empty pattern lists compile: {error}"))
    }
}

impl PartialEq for PackageSelection {
    fn eq(&self, other: &Self) -> bool {
        self.include == other.include && self.exclude == other.exclude
    }
}

impl Eq for PackageSelection {}

#[cfg(test)]
mod tests {
    use rift_protocol::dependencies::{DependenciesConfiguration, PackageNamePattern};

    use super::super::fixture::identity;
    use super::PackageSelection;
    use crate::workspace::WorkspaceIndexViolation;

    fn selection(include: &[&str], exclude: &[&str]) -> PackageSelection {
        let pattern = |value: &&str| PackageNamePattern((*value).to_owned());
        let configuration = DependenciesConfiguration {
            include: include.iter().map(pattern).collect(),
            exclude: exclude.iter().map(pattern).collect(),
            ..DependenciesConfiguration::default()
        };
        PackageSelection::compile(&configuration).expect("valid globs")
    }

    #[test]
    fn test_empty_lists_select_every_package() {
        let every = PackageSelection::default();
        assert!(every.selects(&identity("cargo", "tokio", "1.53.1")));
        assert!(every.selects(&identity("npm", "@types/node", "22.0.0")));
        assert!(every.selects(&identity("stdlib", "rust", "1.90.0")));
        assert!(every.include().is_empty());
        assert!(every.exclude().is_empty());
        assert_eq!(every, selection(&[], &[]));
    }

    #[test]
    fn test_include_selects_matches_over_manager_and_name() {
        let cargo_only = selection(&["cargo/*"], &[]);
        assert!(cargo_only.selects(&identity("cargo", "tokio", "1.53.1")));
        assert!(!cargo_only.selects(&identity("npm", "tokio", "1.0.0")));
        assert!(!cargo_only.selects(&identity("stdlib", "rust", "1.90.0")));

        let scoped = selection(&["npm/@types/*", "stdlib/*"], &[]);
        assert!(scoped.selects(&identity("npm", "@types/node", "22.0.0")));
        assert!(!scoped.selects(&identity("npm", "typescript", "5.0.0")));
        assert!(scoped.selects(&identity("stdlib", "rust", "1.90.0")));

        let exact = selection(&["cargo/helper"], &[]);
        assert!(exact.selects(&identity("cargo", "helper", "0.1.0")));
        assert!(!exact.selects(&identity("cargo", "helper-macros", "0.1.0")));
    }

    #[test]
    fn test_exclude_drops_a_package_include_selected() {
        let without_helper = selection(&[], &["cargo/helper"]);
        assert!(!without_helper.selects(&identity("cargo", "helper", "0.1.0")));
        assert!(without_helper.selects(&identity("cargo", "tokio", "1.53.1")));

        let both = selection(&["cargo/*"], &["cargo/helper"]);
        assert!(!both.selects(&identity("cargo", "helper", "0.1.0")));
        assert!(both.selects(&identity("cargo", "tokio", "1.53.1")));
        assert_eq!(both.include(), ["cargo/*"]);
        assert_eq!(both.exclude(), ["cargo/helper"]);
    }

    #[test]
    fn test_selections_compare_by_their_pattern_lists() {
        assert_eq!(selection(&["cargo/*"], &[]), selection(&["cargo/*"], &[]));
        assert_ne!(selection(&["cargo/*"], &[]), selection(&[], &["cargo/*"]));
        assert_ne!(selection(&[], &[]), selection(&[], &["cargo/helper"]));
        let cloned = selection(&["stdlib/*"], &["cargo/helper"]);
        assert_eq!(cloned.clone(), cloned);
    }

    #[test]
    fn test_an_invalid_glob_refuses_compilation() {
        let configuration = DependenciesConfiguration {
            include: vec![PackageNamePattern("cargo/[".to_owned())],
            ..DependenciesConfiguration::default()
        };
        let error = PackageSelection::compile(&configuration).expect_err("an unclosed class");
        assert_eq!(
            error.fault().violation(),
            WorkspaceIndexViolation::SourcePatternInvalid
        );
    }
}
