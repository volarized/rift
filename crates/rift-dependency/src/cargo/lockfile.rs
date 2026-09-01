//! The lockfile path: the registry and git packages `Cargo.lock` states, rooted in Cargo's caches.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rift_protocol::read::ProjectPath;
use serde::Deserialize;

use super::{CARGO_LOCK_FILE_NAME, CARGO_MANAGER, rust_language};
use crate::catalog::{CatalogEntry, package_identity};
use crate::manifest::{LockfileFailure, ResolutionBuilder, read_lockfile};
use crate::resolver::{DIRECTORY_ENTRIES_MAX, Inspector};

/// The lockfile `source` prefix of a package fetched from a registry.
const REGISTRY_SOURCE_PREFIX: &str = "registry+";
/// The lockfile `source` prefix of a package fetched from a git repository.
const GIT_SOURCE_PREFIX: &str = "git+";
/// The environment variable naming Cargo's home directory.
const CARGO_HOME_VARIABLE: &str = "CARGO_HOME";
/// Cargo's home below the user's home when `CARGO_HOME` is unset.
const CARGO_HOME_DIRECTORY_NAME: &str = ".cargo";
/// Registry package sources below Cargo's home, one directory per index.
const REGISTRY_SOURCE_PATH: &str = "registry/src";
/// Git checkouts below Cargo's home, one directory per repository, then per revision.
const GIT_CHECKOUTS_PATH: &str = "git/checkouts";
/// The repository suffix a git URL may carry and a checkout directory name drops.
const GIT_REPOSITORY_SUFFIX: &str = ".git";
/// The leading characters of a git revision that name its checkout directory.
const GIT_CHECKOUT_REVISION_CHARS: usize = 7;

/// The `Cargo.lock` document, the fields this resolver reads.
#[derive(Deserialize)]
struct Lockfile {
    #[serde(default)]
    package: Vec<LockedPackage>,
}

/// One `[[package]]` table of the lockfile.
///
/// A package without `source` is a workspace member or a path dependency; the lockfile
/// does not tell the two apart, so neither is cataloged. Its `dependencies` still decide
/// which packages the workspace declares directly.
#[derive(Deserialize)]
struct LockedPackage {
    name: String,
    version: String,
    source: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

/// Catalogs the registry and git packages the `Cargo.lock` beside a manifest states.
pub(super) fn resolve_lockfile(
    directory: &Path,
    manifest: &ProjectPath,
    inspector: &mut dyn Inspector,
    answer: &mut ResolutionBuilder,
) {
    let observed = read_lockfile(directory, CARGO_LOCK_FILE_NAME, inspector);
    match observed.and_then(|bytes| parse_lockfile(&bytes)) {
        Ok(lockfile) => {
            let cache = CargoCache::observe(inspector);
            answer.entries(lockfile_entries(&lockfile, &cache, inspector));
        }
        Err(failure) => {
            let manifest_path = &manifest.0;
            answer.degradation(format!("{manifest_path}: {failure}; no packages cataloged"));
        }
    }
}

/// Parses `Cargo.lock` bytes, naming the parser's message when they are not its document.
fn parse_lockfile(bytes: &[u8]) -> Result<Lockfile, LockfileFailure> {
    toml::from_slice(bytes).map_err(|error| {
        LockfileFailure::unparsable(CARGO_LOCK_FILE_NAME, error.message().to_owned())
    })
}

/// Where a locked package's bytes came from, read from its `source` prefix.
#[derive(Clone, Copy, Debug)]
enum LockedSource<'a> {
    /// A registry package, cached under `registry/src/<index>/<name>-<version>`.
    Registry,
    /// A git package; carries the whole source so the checkout can be located.
    Git(&'a str),
}

/// Classifies one lockfile `source`; a source of any other kind is not cataloged.
fn locked_source(source: &str) -> Option<LockedSource<'_>> {
    if source.starts_with(REGISTRY_SOURCE_PREFIX) {
        Some(LockedSource::Registry)
    } else if source.starts_with(GIT_SOURCE_PREFIX) {
        Some(LockedSource::Git(source))
    } else {
        None
    }
}

/// One entry per registry or git package, with the cache directory holding its source.
///
/// Each registry package probes every index directory in name order until one holds
/// it, so the work is at most the package count times `DIRECTORY_ENTRIES_MAX` probes.
fn lockfile_entries(
    lockfile: &Lockfile,
    cache: &CargoCache,
    inspector: &mut dyn Inspector,
) -> Vec<CatalogEntry> {
    let direct = declared_dependencies(lockfile);
    let mut entries = Vec::with_capacity(lockfile.package.len());
    for package in &lockfile.package {
        let Some(source) = package.source.as_deref().and_then(locked_source) else {
            continue;
        };
        let source_root = match source {
            LockedSource::Registry => {
                cache.registry_root(inspector, &package.name, &package.version)
            }
            LockedSource::Git(source) => cache.git_root(inspector, source),
        };
        entries.push(CatalogEntry::dependency(
            package_identity(CARGO_MANAGER, &package.name, &package.version),
            rust_language(),
            source_root,
            direct.contains(package.name.as_str()),
        ));
    }
    entries
}

/// The names the workspace's own packages depend on.
///
/// A lockfile dependency is spelled `name`, `name version`, or `name version (source)`;
/// the first whitespace-separated token is the name.
fn declared_dependencies(lockfile: &Lockfile) -> BTreeSet<&str> {
    lockfile
        .package
        .iter()
        .filter(|package| package.source.is_none())
        .flat_map(|package| package.dependencies.iter())
        .filter_map(|dependency| dependency.split_whitespace().next())
        .collect()
}

/// Cargo's package caches on this machine, as the inspector listed them.
#[derive(Debug, Default)]
struct CargoCache {
    registry_source: Option<PathBuf>,
    index_directories: Vec<String>,
    checkouts: Option<PathBuf>,
    checkout_directories: Vec<String>,
}

impl CargoCache {
    /// Lists the registry index and git checkout directories below Cargo's home.
    ///
    /// With no home to derive, every probe answers no root.
    fn observe(inspector: &mut dyn Inspector) -> Self {
        let Some(home) = cargo_home(inspector) else {
            return Self::default();
        };
        let registry_source = home.join(REGISTRY_SOURCE_PATH);
        let checkouts = home.join(GIT_CHECKOUTS_PATH);
        Self {
            index_directories: inspector.list_directory(&registry_source, DIRECTORY_ENTRIES_MAX),
            checkout_directories: inspector.list_directory(&checkouts, DIRECTORY_ENTRIES_MAX),
            registry_source: Some(registry_source),
            checkouts: Some(checkouts),
        }
    }

    /// The first index directory holding `<name>-<version>`, probed in name order.
    fn registry_root(
        &self,
        inspector: &mut dyn Inspector,
        name: &str,
        version: &str,
    ) -> Option<PathBuf> {
        let registry_source = self.registry_source.as_ref()?;
        let directory_name = format!("{name}-{version}");
        self.index_directories
            .iter()
            .map(|index| registry_source.join(index).join(&directory_name))
            .find(|candidate| inspector.directory_exists(candidate))
    }

    /// The first checkout of the source's repository holding its revision.
    fn git_root(&self, inspector: &mut dyn Inspector, source: &str) -> Option<PathBuf> {
        let checkouts = self.checkouts.as_ref()?;
        let (repository, revision) = git_source_parts(source)?;
        let short_revision = revision
            .get(..GIT_CHECKOUT_REVISION_CHARS)
            .unwrap_or(revision);
        self.checkout_directories
            .iter()
            .filter(|entry| is_repository_checkout(entry, repository))
            .map(|entry| checkouts.join(entry).join(short_revision))
            .find(|candidate| inspector.directory_exists(candidate))
    }
}

/// Cargo's home: `CARGO_HOME`, else `.cargo` below the user's home.
fn cargo_home(inspector: &mut dyn Inspector) -> Option<PathBuf> {
    match inspector.environment(CARGO_HOME_VARIABLE) {
        Some(home) => Some(PathBuf::from(home)),
        None => inspector
            .home_directory()
            .map(|home| home.join(CARGO_HOME_DIRECTORY_NAME)),
    }
}

/// The repository name and revision a `git+` source names; absent without a revision.
///
/// The repository name is the last path segment of the URL before any `?` query,
/// with a trailing `.git` dropped; the revision follows the `#`.
fn git_source_parts(source: &str) -> Option<(&str, &str)> {
    let locator = source.strip_prefix(GIT_SOURCE_PREFIX)?;
    let (url, revision) = locator.split_once('#')?;
    let url = url.split_once('?').map_or(url, |(url, _)| url);
    let url = url.trim_end_matches('/');
    let repository = url.rsplit_once('/').map_or(url, |(_, name)| name);
    let repository = repository
        .strip_suffix(GIT_REPOSITORY_SUFFIX)
        .unwrap_or(repository);
    (!repository.is_empty() && !revision.is_empty()).then_some((repository, revision))
}

/// Whether a checkout directory name is `<repository>-<hash>`.
fn is_repository_checkout(entry: &str, repository: &str) -> bool {
    entry
        .strip_prefix(repository)
        .is_some_and(|rest| rest.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::super::fixture::{
        CARGO_HOME, INDEX_DIRECTORY, REGISTRY_SOURCE, ROOT, RUFF_REVISION, RUFF_SOURCE,
        WORKSPACE_LOCKFILE, entry, project, resolve,
    };
    use super::{CargoCache, git_source_parts};
    use crate::fixture::RecordedInspector;
    use crate::resolver::LOCKFILE_BYTES_MAX;

    #[test]
    fn test_resolve_lockfile_absent_degrades_without_entries() {
        let mut inspector = RecordedInspector::default().with_directory(ROOT);

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations[1],
            "Cargo.toml: no Cargo.lock beside it; no packages cataloged"
        );
        assert!(resolution.entries.is_empty());
        assert_eq!(
            resolution.inputs,
            [project("Cargo.toml"), project("Cargo.lock")]
        );
    }

    #[test]
    fn test_resolve_lockfile_over_bound_degrades_without_entries() {
        let oversized = vec![b'#'; usize::try_from(LOCKFILE_BYTES_MAX).expect("bound fits") + 1];
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/Cargo.lock"), oversized);

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations[1],
            format!(
                "Cargo.toml: Cargo.lock holds {} bytes, past the {LOCKFILE_BYTES_MAX} byte bound; \
                 no packages cataloged",
                LOCKFILE_BYTES_MAX + 1
            )
        );
        assert!(resolution.entries.is_empty());
    }

    #[test]
    fn test_resolve_lockfile_unparsable_degrades_without_entries() {
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/Cargo.lock"), "[[package]]\nname = ");

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        let degradation = &resolution.degradations[1];
        assert!(
            degradation.starts_with("Cargo.toml: Cargo.lock could not be parsed: "),
            "{degradation}"
        );
        assert!(
            degradation.ends_with("; no packages cataloged"),
            "{degradation}"
        );
        assert!(resolution.entries.is_empty());
    }

    #[test]
    fn test_resolve_cargo_home_unset_falls_back_to_home_cargo() {
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/Cargo.lock"), WORKSPACE_LOCKFILE)
            .with_home("/home/user")
            .with_directory(format!(
                "/home/user/.cargo/registry/src/{INDEX_DIRECTORY}/serde-1.0.228"
            ));

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        assert_eq!(
            entry(&resolution, "serde").source_root(),
            Some(Path::new(
                "/home/user/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.228"
            ))
        );
        assert!(inspector.asked.contains(&"home".to_owned()));
        assert!(
            inspector
                .asked
                .contains(&"list /home/user/.cargo/registry/src".to_owned())
        );
    }

    #[test]
    fn test_resolve_home_unset_catalogs_without_roots() {
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/Cargo.lock"), WORKSPACE_LOCKFILE);

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        assert_eq!(resolution.entries.len(), 4);
        assert!(
            resolution
                .entries
                .iter()
                .all(|entry| entry.source_root().is_none())
        );
        assert!(entry(&resolution, "serde").is_direct());
        assert!(!inspector.asked.iter().any(|line| line.starts_with("list ")));
    }

    #[test]
    fn test_git_source_parts_strips_query_and_git_suffix() {
        assert_eq!(git_source_parts(RUFF_SOURCE), Some(("ruff", RUFF_REVISION)));
        assert_eq!(
            git_source_parts("git+https://example.com/org/tool.git/#abc1234"),
            Some(("tool", "abc1234"))
        );
        assert_eq!(
            git_source_parts("git+https://example.com/org/tool"),
            None,
            "no revision"
        );
        assert_eq!(git_source_parts("git+#abc"), None, "no repository");
        assert_eq!(git_source_parts(REGISTRY_SOURCE), None, "not a git source");
    }

    #[test]
    fn test_git_root_uses_whole_revision_when_shorter_than_checkout_name() {
        let cache = CargoCache {
            checkouts: Some(PathBuf::from(format!("{CARGO_HOME}/git/checkouts"))),
            checkout_directories: vec!["other-1".to_owned(), "tool-abc".to_owned()],
            ..CargoCache::default()
        };
        let mut inspector = RecordedInspector::default()
            .with_directory(format!("{CARGO_HOME}/git/checkouts/tool-abc/ab12"));

        let root = cache.git_root(&mut inspector, "git+https://example.com/tool#ab12");

        assert_eq!(
            root,
            Some(PathBuf::from("/cargo-home/git/checkouts/tool-abc/ab12"))
        );
        assert_eq!(
            inspector.asked,
            ["exists /cargo-home/git/checkouts/tool-abc/ab12"],
            "a checkout of another repository is never probed"
        );
    }
}
