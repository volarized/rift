//! What the npm and Bun resolvers share: one package namespace, one install layout.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ignore::overrides::{Override, OverrideBuilder};
use rift_protocol::dependencies::{
    PackageAvailability, PackageContextEntry, PackageSelector, REQUIREMENT_ANY,
};
use rift_protocol::read::PackageIdentity;
use serde::Deserialize;

use crate::context::{InstallFolder, InstallLocation, is_whole_version};
use crate::manifest::{StaticFileFailure, WorkspacePaths};
use crate::resolver::StaticInputs;

/// The package namespace every npm and Bun entry belongs to: both install from the npm
/// registry, so one package has one identity whichever tool pinned it.
pub(crate) const NPM_MANAGER: &str = "npm";
/// The manifest file name both resolvers claim.
pub(crate) const PACKAGE_MANIFEST_FILE_NAME: &str = "package.json";
/// The directory installed packages sit under, beside the manifest.
pub(crate) const NODE_MODULES_DIRECTORY_NAME: &str = "node_modules";

/// The install folder of the npm package `name@version`, at `install_path` below
/// `directory`, the folder holding the lockfile.
///
/// `install_path` is the lockfile's own spelling, `node_modules/<name>` or a nested
/// `node_modules/<parent>/node_modules/<name>`, with forward slashes: every nested copy
/// gets a folder of its own, whatever version it pins.
#[must_use]
pub(crate) fn installed_folder(
    directory: &Path,
    install_path: &str,
    name: &str,
    version: &str,
) -> InstallFolder {
    let folder: PathBuf = install_path
        .split('/')
        .fold(directory.to_path_buf(), |folder, segment| {
            folder.join(segment)
        });
    InstallFolder {
        package: PackageIdentity {
            manager: NPM_MANAGER.to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
        },
        location: InstallLocation::Path(folder),
    }
}

/// The character opening a scoped package name, `@scope/name`.
pub(crate) const SCOPE_PREFIX: char = '@';
/// The character between a package name and its version in a reference.
pub(crate) const NAME_VERSION_SEPARATOR: char = '@';
/// The version prefix of an alias: the real `<name>@<version>` follows it.
pub(crate) const ALIAS_VERSION_PREFIX: &str = "npm:";
/// The version prefixes naming a path relative to the manifest: a directory or an
/// archive, copied or linked.
const PATH_VERSION_PREFIXES: [&str; 3] = ["file:", "link:", "portal:"];
/// The version prefixes naming a tarball URL.
const URL_VERSION_PREFIXES: [&str; 2] = ["http:", "https:"];
/// The version prefix naming one of the package manager's own workspace members.
const WORKSPACE_VERSION_PREFIX: &str = "workspace:";
/// The version prefix naming a range the package manager's workspace file catalogs,
/// `pnpm-workspace.yaml` or the root `package.json`, which the pass does not read.
const CATALOG_VERSION_PREFIX: &str = "catalog:";
/// The version prefixes naming a git repository.
const GIT_VERSION_PREFIXES: [&str; 3] = ["git:", "git+", "github:"];
/// The file name endings of a packed package. The local index reads no archive, so one
/// inside the root is not project source.
const ARCHIVE_SUFFIXES: [&str; 3] = [".tgz", ".tar.gz", ".tar"];

/// The separator a repository shorthand carries: `user/repo`, with an optional `#ref`.
/// No registry version range holds one, so a value carrying it names a repository.
const SHORTHAND_SEPARATOR: char = '/';

/// Splits `<name>@<version>` at the first `@` past a scope's own.
///
/// A name carries no `@` but the scope's, so `@types/react@19.2.18` names `@types/react`,
/// and a version spelled as a URL keeps every `@` it carries. Absent when either side
/// is empty.
pub(crate) fn split_reference(reference: &str) -> Option<(&str, &str)> {
    let scope_bytes = usize::from(reference.starts_with(SCOPE_PREFIX));
    let separator = reference[scope_bytes..].find(NAME_VERSION_SEPARATOR)? + scope_bytes;
    let name = &reference[..separator];
    let version = &reference[separator + NAME_VERSION_SEPARATOR.len_utf8()..];
    let name_present = !name.is_empty();
    let version_present = !version.is_empty();
    (name_present && version_present).then_some((name, version))
}

/// Whether one version text names a member of the package manager's workspace.
#[must_use]
pub(crate) fn is_workspace_version(version: &str) -> bool {
    version.starts_with(WORKSPACE_VERSION_PREFIX)
}

/// Whether a global package index can answer for the package one npm version text
/// names, read from the manifest or lockfile in `directory`. `None` for project source:
/// a directory inside the root.
///
/// A tarball URL is a `url` entry, and a repository or a bare `user/repo` shorthand a
/// `git` one. A path outside the root is a `path` entry, and so is a `workspace:` member,
/// whose manifest may stand outside the root: a caller that knows the claimed manifests'
/// names leaves out one they declare before asking. A `catalog:` range names a registry
/// package.
pub(crate) fn version_availability(
    version: &str,
    directory: &Path,
    workspace: &WorkspacePaths,
    inputs: &mut dyn StaticInputs,
) -> Option<PackageAvailability> {
    if is_workspace_version(version) {
        return Some(PackageAvailability::Path);
    }
    if version.starts_with(CATALOG_VERSION_PREFIX) {
        return Some(PackageAvailability::Canonical);
    }
    if let Some(path) = PATH_VERSION_PREFIXES
        .iter()
        .find_map(|prefix| version.strip_prefix(prefix))
    {
        return path_availability(path, directory, workspace, inputs);
    }
    if URL_VERSION_PREFIXES
        .iter()
        .any(|prefix| version.starts_with(prefix))
    {
        return Some(PackageAvailability::Url);
    }
    let prefixed = GIT_VERSION_PREFIXES
        .iter()
        .any(|prefix| version.starts_with(prefix));
    if prefixed || version.contains(SHORTHAND_SEPARATOR) {
        Some(PackageAvailability::Git)
    } else {
        Some(PackageAvailability::Canonical)
    }
}

/// Whether a global package index can answer for the package at `path`, relative to
/// `directory`. `None` for a directory inside the root, which is project source; an
/// archive is a `path` entry wherever it stands, since the local index reads no archive.
pub(crate) fn path_availability(
    path: &str,
    directory: &Path,
    workspace: &WorkspacePaths,
    inputs: &mut dyn StaticInputs,
) -> Option<PackageAvailability> {
    let archive = ARCHIVE_SUFFIXES.iter().any(|suffix| path.ends_with(suffix));
    if !archive && workspace.contains(&directory.join(path), inputs) {
        None
    } else {
        Some(PackageAvailability::Path)
    }
}

/// The selector one npm version text states.
///
/// npm reads a bare `1.2.3` as that exact release, so a whole version pins and every
/// range, tag, or locator is a requirement. A `catalog:` range is the requirement `>=0`.
pub(crate) fn npm_selector(version: &str) -> PackageSelector {
    if version.starts_with(CATALOG_VERSION_PREFIX) {
        return PackageSelector::Requirement(REQUIREMENT_ANY.to_owned());
    }
    if is_whole_version(version) {
        PackageSelector::Version(version.to_owned())
    } else {
        PackageSelector::Requirement(version.to_owned())
    }
}

/// The `package.json` document: its name, its `workspaces` globs, and the dependency
/// maps the static context reads.
#[derive(Deserialize)]
pub(crate) struct PackageManifest {
    name: Option<String>,
    workspaces: Option<Workspaces>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
}

/// The `workspaces` value: the globs alone, or a table carrying them under `packages`.
#[derive(Deserialize)]
#[serde(untagged)]
enum Workspaces {
    Globs(Vec<String>),
    Table {
        #[serde(default)]
        packages: Vec<String>,
    },
}

/// The glob npm leaves out of every `workspaces` match
/// (`@npmcli/map-workspaces` 4.0.2 `lib/index.js:113`).
const NODE_MODULES_GLOB: &str = "!**/node_modules/**";

impl PackageManifest {
    /// The `name` this manifest declares.
    pub(crate) fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The name npm gives a workspace member standing in `directory`: its `name`, else
    /// the folder name with its `@scope` parent.
    pub(crate) fn member_name(&self, directory: &str) -> String {
        if let Some(name) = self.name() {
            return name.to_owned();
        }
        let mut segments = directory.rsplit('/');
        let folder = segments.next().unwrap_or(directory);
        match segments.next() {
            Some(parent) if parent.starts_with(SCOPE_PREFIX) => format!("{parent}/{folder}"),
            _ => folder.to_owned(),
        }
    }

    /// The `workspaces` globs compiled against `directory`, the folder holding this
    /// manifest; absent when it declares none.
    ///
    /// npm strips a leading `./` or `/`, reads an odd count of leading `!` as a negation,
    /// and never matches below `node_modules` (`@npmcli/map-workspaces` 4.0.2
    /// `lib/index.js:12-22`, `:113`). Each glob is anchored at `directory`, and a later
    /// glob decides over an earlier one, as in `.gitignore`.
    ///
    /// # Errors
    ///
    /// Returns the matcher's error when a glob is not valid.
    pub(crate) fn workspace_globs(
        &self,
        directory: &Path,
    ) -> Result<Option<Override>, ignore::Error> {
        let Some(Workspaces::Globs(globs) | Workspaces::Table { packages: globs }) =
            &self.workspaces
        else {
            return Ok(None);
        };
        if globs.is_empty() {
            return Ok(None);
        }
        let mut builder = OverrideBuilder::new(directory);
        for glob in globs {
            let kept = glob.trim_start_matches('!');
            let negation = if (glob.len() - kept.len()) % 2 == 1 {
                "!"
            } else {
                ""
            };
            let relative = kept
                .strip_prefix('.')
                .filter(|rest| rest.starts_with('/'))
                .unwrap_or(kept)
                .trim_start_matches('/');
            builder.add(&format!("{negation}/{relative}"))?;
        }
        builder.add(NODE_MODULES_GLOB)?;
        builder.build().map(Some)
    }

    /// One entry per declared dependency, in map then key order. `availability` answers
    /// each package name and version text, `None` for project source, which is left out.
    pub(crate) fn declared<'a>(
        &'a self,
        mut availability: impl FnMut(&str, &str) -> Option<PackageAvailability> + 'a,
    ) -> impl Iterator<Item = PackageContextEntry> + 'a {
        [
            &self.dependencies,
            &self.dev_dependencies,
            &self.optional_dependencies,
        ]
        .into_iter()
        .flat_map(|map| map.iter())
        .filter_map(move |(key, version)| {
            let (name, version) = declared_version(key, version);
            let availability = availability(name, version)?;
            Some(PackageContextEntry::new(
                NPM_MANAGER,
                name,
                npm_selector(version),
                availability,
            ))
        })
    }
}

/// Parses `package.json` bytes, naming the parser's message when they are not its
/// document.
pub(crate) fn parse_package_manifest(bytes: &[u8]) -> Result<PackageManifest, StaticFileFailure> {
    serde_json::from_slice(bytes).map_err(|error| {
        StaticFileFailure::unparsable(PACKAGE_MANIFEST_FILE_NAME, error.to_string())
    })
}

/// The package name and version text one `package.json` dependency value declares. An
/// `npm:` value aliases another package, and the pair names the package it aliases.
fn declared_version<'a>(key: &'a str, version: &'a str) -> (&'a str, &'a str) {
    version
        .strip_prefix(ALIAS_VERSION_PREFIX)
        .and_then(split_reference)
        .unwrap_or((key, version))
}
