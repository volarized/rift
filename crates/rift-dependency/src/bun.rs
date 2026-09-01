//! The Bun resolver: npm packages as `bun.lock` pins them and `node_modules` holds them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str::Split;

use rift_protocol::read::{Language, ProjectPath};
use serde::Deserialize;
use serde_json_lenient::Value;

use crate::catalog::{CatalogEntry, Resolution};
use crate::manifest::{
    LockfileFailure, ResolutionBuilder, file_beside, manifest_directory_path, read_lockfile,
};
use crate::node::{self, NODE_MODULES_DIRECTORY_NAME, TYPESCRIPT_PACKAGE_NAME};
use crate::resolver::{DependencyResolver, Inspector, ResolutionRequest, ResolverName};

/// The lockfile Bun keeps beside a workspace root manifest.
const BUN_LOCK_FILE_NAME: &str = "bun.lock";
/// The newest `lockfileVersion` whose shape this resolver has read. A newer lockfile is
/// read the same way, and the assumption is reported.
const LOCKFILE_VERSION_MAX: u64 = 1;
/// The version prefix naming one of the workspace's own packages, never cataloged.
const WORKSPACE_VERSION_PREFIX: &str = "workspace:";
/// The version prefix of an alias: the real `<name>@<version>` follows it.
const ALIAS_VERSION_PREFIX: &str = "npm:";
/// The character opening a scoped package name, `@scope/name`.
const SCOPE_PREFIX: char = '@';
/// The character between a package name and its version in a lockfile reference.
const NAME_VERSION_SEPARATOR: char = '@';
/// The character between parent and child in a nested `packages` key, and between a
/// scope and its name.
const NESTING_SEPARATOR: char = '/';
/// Names one `packages` key may nest, at most; a deeper key spells no install path.
const NESTING_DEPTH_MAX: usize = 32;

/// The resolver for npm packages Bun installed, answering from `bun.lock`.
#[derive(Debug, Default)]
pub struct BunResolver;

impl BunResolver {
    /// The Bun resolver. It holds no state, so one instance serves every workspace.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl DependencyResolver for BunResolver {
    fn name(&self) -> ResolverName {
        ResolverName::Bun
    }

    fn manager(&self) -> &'static str {
        node::NPM_MANAGER
    }

    fn language(&self) -> Language {
        node::typescript_language()
    }

    fn manifest_file_name(&self) -> &'static str {
        node::PACKAGE_MANIFEST_FILE_NAME
    }

    /// Catalogs the packages the `bun.lock` beside each manifest pins.
    ///
    /// Every listed manifest is an input. A manifest with `bun.lock` beside it is a
    /// lockfile root: the lockfile is an input too, and every package it pins is
    /// cataloged from the `node_modules` beside that manifest. A manifest with no
    /// `bun.lock` beside it contributes nothing more, silently: a Bun workspace keeps
    /// one lockfile at its root, whose `workspaces` map restates every member's
    /// dependencies, so a member manifest is covered by the nearest listed ancestor
    /// that is a lockfile root; and a manifest beside `package-lock.json` instead
    /// belongs to the npm resolver, which reports the case where neither lockfile
    /// stands. Both silent cases answer alike, so the ancestor chain is never walked.
    /// Two lockfile roots each catalog their own packages; `DependencyCatalog::assemble`
    /// merges an identity the two share. Entries stop at `PACKAGES_MAX` and the drop is
    /// reported. The work is one file read per manifest plus one directory probe per
    /// pinned package.
    fn resolve(
        &self,
        request: &ResolutionRequest<'_>,
        inspector: &mut dyn Inspector,
    ) -> Resolution {
        let mut answer = ResolutionBuilder::default();
        for manifest in request.manifests {
            answer.input(manifest.clone());
        }
        for manifest in request.manifests {
            resolve_manifest(request.root, manifest, inspector, &mut answer);
        }
        answer.build()
    }
}

/// Catalogs the packages of the `bun.lock` beside one manifest; silent when none stands there.
fn resolve_manifest(
    root: &Path,
    manifest: &ProjectPath,
    inspector: &mut dyn Inspector,
    answer: &mut ResolutionBuilder,
) {
    let directory = manifest_directory_path(root, manifest);
    let observed = match read_lockfile(&directory, BUN_LOCK_FILE_NAME, inspector) {
        Err(failure) if failure.is_absent() => return,
        observed => observed,
    };
    answer.input(file_beside(manifest, BUN_LOCK_FILE_NAME));
    match observed.and_then(|bytes| parse_lockfile(&bytes)) {
        Ok(lockfile) => catalog_lockfile(&lockfile, manifest, &directory, inspector, answer),
        Err(failure) => {
            let manifest_path = &manifest.0;
            answer.degradation(format!("{manifest_path}: {failure}; no packages cataloged"));
        }
    }
}

/// Parses the JSONC document Bun writes: comments and trailing commas are accepted.
fn parse_lockfile(bytes: &[u8]) -> Result<BunLock, LockfileFailure> {
    serde_json_lenient::from_slice(bytes)
        .map_err(|error| LockfileFailure::unparsable(BUN_LOCK_FILE_NAME, error.to_string()))
}

/// The `bun.lock` document, the fields this resolver reads.
#[derive(Deserialize)]
struct BunLock {
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u64,
    /// The workspace's own packages keyed by directory, the root under `""`.
    #[serde(default)]
    workspaces: BTreeMap<String, Workspace>,
    /// Every pinned package keyed by its install location below `node_modules`.
    #[serde(default)]
    packages: BTreeMap<String, LockedPackage>,
}

impl BunLock {
    /// Every name the workspace's packages declare across their three dependency maps.
    fn declared_names(&self) -> BTreeSet<&str> {
        self.workspaces
            .values()
            .flat_map(Workspace::declared_names)
            .collect()
    }

    /// The `typescript` version the hoisted package pins, when it is that package.
    fn typescript_version(&self) -> Option<&str> {
        match self.packages.get(TYPESCRIPT_PACKAGE_NAME)?.pin() {
            Pin::Package { name, version, .. } if name == TYPESCRIPT_PACKAGE_NAME => Some(version),
            _ => None,
        }
    }
}

/// One workspace package's declared dependencies, as the lockfile restates them.
#[derive(Deserialize)]
struct Workspace {
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
}

impl Workspace {
    /// The names this package declares, across its three dependency maps.
    fn declared_names(&self) -> impl Iterator<Item = &str> {
        self.dependencies
            .keys()
            .chain(self.dev_dependencies.keys())
            .chain(self.optional_dependencies.keys())
            .map(String::as_str)
    }
}

/// One `packages` tuple: its first element spells `<name>@<version>`; the registry,
/// metadata, and integrity after it are not read.
#[derive(Deserialize)]
#[serde(transparent)]
struct LockedPackage(Vec<Value>);

impl LockedPackage {
    /// What the tuple pins.
    fn pin(&self) -> Pin<'_> {
        match self
            .0
            .first()
            .and_then(Value::as_str)
            .and_then(split_reference)
        {
            Some((spelled, version)) => pinned(spelled, version),
            None => Pin::Malformed,
        }
    }
}

/// What one `packages` tuple pins.
#[derive(Debug, Eq, PartialEq)]
enum Pin<'a> {
    /// The tuple opens with no `<name>@<version>` reference; counted, never cataloged.
    Malformed,
    /// One of the workspace's own packages; never cataloged.
    Workspace,
    /// A package to catalog. `spelled` is the name the key and the manifests use, which
    /// decides whether the package is direct; an alias resolves to another `name`.
    Package {
        spelled: &'a str,
        name: &'a str,
        version: &'a str,
    },
}

/// What a reference's version text pins under `spelled`.
fn pinned<'a>(spelled: &'a str, version: &'a str) -> Pin<'a> {
    if version.starts_with(WORKSPACE_VERSION_PREFIX) {
        return Pin::Workspace;
    }
    let Some(aliased) = version.strip_prefix(ALIAS_VERSION_PREFIX) else {
        return Pin::Package {
            spelled,
            name: spelled,
            version,
        };
    };
    match split_reference(aliased) {
        Some((name, version)) => Pin::Package {
            spelled,
            name,
            version,
        },
        None => Pin::Malformed,
    }
}

/// Splits `<name>@<version>` at the first `@` past a scope's own.
///
/// A name carries no `@` but the scope's, so `@types/react@19.2.18` names `@types/react`,
/// and a version spelled as a URL keeps every `@` it carries. Absent when either side
/// is empty.
fn split_reference(reference: &str) -> Option<(&str, &str)> {
    let scope_bytes = usize::from(reference.starts_with(SCOPE_PREFIX));
    let separator = reference[scope_bytes..].find(NAME_VERSION_SEPARATOR)? + scope_bytes;
    let name = &reference[..separator];
    let version = &reference[separator + NAME_VERSION_SEPARATOR.len_utf8()..];
    let name_present = !name.is_empty();
    let version_present = !version.is_empty();
    (name_present && version_present).then_some((name, version))
}

/// Catalogs every package one parsed lockfile pins, rooted in the `node_modules` beside
/// the manifest.
fn catalog_lockfile(
    lockfile: &BunLock,
    manifest: &ProjectPath,
    directory: &Path,
    inspector: &mut dyn Inspector,
    answer: &mut ResolutionBuilder,
) {
    let manifest_path = &manifest.0;
    if lockfile.lockfile_version > LOCKFILE_VERSION_MAX {
        answer.degradation(format!(
            "{manifest_path}: {BUN_LOCK_FILE_NAME} states lockfileVersion {}, newer than the \
             {LOCKFILE_VERSION_MAX} this resolver reads; read as version {LOCKFILE_VERSION_MAX}",
            lockfile.lockfile_version
        ));
    }
    let declared = lockfile.declared_names();
    let mut malformed_count = 0_usize;
    for (key, package) in &lockfile.packages {
        match package.pin() {
            Pin::Malformed => malformed_count += 1,
            Pin::Workspace => {}
            Pin::Package {
                spelled,
                name,
                version,
            } => {
                let direct = declared.contains(spelled);
                answer.entry(package_entry(
                    inspector, directory, key, name, version, direct,
                ));
            }
        }
    }
    if let Some(version) = lockfile.typescript_version() {
        answer.entry(node::typescript_library_entry(
            inspector, directory, version,
        ));
    }
    if malformed_count > 0 {
        answer.degradation(format!(
            "{manifest_path}: {malformed_count} {BUN_LOCK_FILE_NAME} packages entries open with \
             no `<name>@<version>` reference; not cataloged"
        ));
    }
}

/// One dependency entry, rooted at the install path its key spells when that directory
/// exists; a key spelling no path gives an entry with no root.
fn package_entry(
    inspector: &mut dyn Inspector,
    directory: &Path,
    key: &str,
    name: &str,
    version: &str,
    direct: bool,
) -> CatalogEntry {
    match install_path(key) {
        Some(path) => node::installed_entry(inspector, directory, &path, name, version, direct),
        None => CatalogEntry::dependency(
            node::npm_identity(name, version),
            node::typescript_language(),
            None,
            direct,
        ),
    }
}

/// The install path a `packages` key spells: `node_modules/<name>` for a hoisted package,
/// and `node_modules/<parent>/node_modules/<name>` below each parent for a nested one.
///
/// The key alone spells the location, whether or not the parent is itself a key.
fn install_path(key: &str) -> Option<String> {
    let names = nested_names(key)?;
    let nesting = format!("{NESTING_SEPARATOR}{NODE_MODULES_DIRECTORY_NAME}{NESTING_SEPARATOR}");
    Some(format!(
        "{NODE_MODULES_DIRECTORY_NAME}{NESTING_SEPARATOR}{}",
        names.join(&nesting)
    ))
}

/// The names a `packages` key nests, outermost first; a scoped name keeps its own `/`.
///
/// Absent when a segment is empty, a scope has no name after it, or the key nests more
/// than `NESTING_DEPTH_MAX` names.
fn nested_names(key: &str) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut segments = key.split(NESTING_SEPARATOR);
    while let Some(segment) = segments.next() {
        if names.len() == NESTING_DEPTH_MAX {
            return None;
        }
        names.push(nested_name(segment, &mut segments)?);
    }
    Some(names)
}

/// One name from a key: `segment` alone, or a scope joined with the segment after it.
fn nested_name(segment: &str, segments: &mut Split<'_, char>) -> Option<String> {
    if segment.is_empty() {
        return None;
    }
    if !segment.starts_with(SCOPE_PREFIX) {
        return Some(segment.to_owned());
    }
    let name = segments.next().filter(|name| !name.is_empty())?;
    Some(format!("{segment}{NESTING_SEPARATOR}{name}"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::catalog::PackageLocation;
    use crate::fixture::RecordedInspector;
    use crate::resolver::{LOCKFILE_BYTES_MAX, PACKAGES_MAX};

    const ROOT: &str = "/workspace";

    /// The `bun.lock` of this repository's live TypeScript fixture, byte for byte.
    const FIXTURE_LOCKFILE: &str = r#"{
  "lockfileVersion": 1,
  "configVersion": 1,
  "workspaces": {
    "": {
      "name": "rift-live-fixture",
      "devDependencies": {
        "typescript": "5.9.3",
        "typescript-language-server": "6.0.0",
      },
    },
  },
  "packages": {
    "typescript": ["typescript@5.9.3", "", { "bin": { "tsc": "bin/tsc", "tsserver": "bin/tsserver" } }, "sha512-jl1vZzPDinLr9eUt3J/t7V6FgNEw9QjvBPdysz9KfQDD41fQrC2Y4vKQdiaUpFT4bXlb1RHhLpp8wtm6M5TgSw=="],

    "typescript-language-server": ["typescript-language-server@6.0.0", "", { "bin": { "typescript-language-server": "lib/cli.mjs" } }, "sha512-LXtzY3UZGfghWA5eRU6/T5j1+YiGRgy14mR3GOKyTKlE1op1TYKQnLVxwBsmnXeDhGLuvzZyIHBAqvrekAITYQ=="],
  }
}
"#;

    fn project(path: &str) -> ProjectPath {
        ProjectPath(path.to_owned())
    }

    fn resolve(manifests: &[&str], inspector: &mut RecordedInspector) -> Resolution {
        let manifests: Vec<ProjectPath> = manifests.iter().map(|path| project(path)).collect();
        let request = ResolutionRequest {
            root: Path::new(ROOT),
            manifests: &manifests,
        };
        BunResolver::new().resolve(&request, inspector)
    }

    /// Every entry as `manager/name@version`, in resolution order.
    fn names(resolution: &Resolution) -> Vec<String> {
        resolution
            .entries
            .iter()
            .map(|entry| {
                let identity = entry.identity();
                format!(
                    "{}/{}@{}",
                    identity.manager, identity.name, identity.version
                )
            })
            .collect()
    }

    /// The entry spelled `name@version`.
    fn entry<'a>(resolution: &'a Resolution, reference: &str) -> &'a CatalogEntry {
        resolution
            .entries
            .iter()
            .find(|entry| {
                let identity = entry.identity();
                format!("{}@{}", identity.name, identity.version) == reference
            })
            .unwrap_or_else(|| panic!("{reference} is cataloged: {:?}", names(resolution)))
    }

    /// A root lockfile declaring `declared` and pinning `packages` as `(key, reference)`.
    fn lockfile(declared: &[&str], packages: &[(&str, &str)]) -> String {
        let dependencies: serde_json::Map<String, serde_json::Value> = declared
            .iter()
            .map(|name| ((*name).to_owned(), json!("*")))
            .collect();
        let packages: serde_json::Map<String, serde_json::Value> = packages
            .iter()
            .map(|(key, reference)| ((*key).to_owned(), json!([reference, "", {}, "sha512-x"])))
            .collect();
        json!({
            "lockfileVersion": 1,
            "configVersion": 1,
            "workspaces": { "": { "name": "app", "dependencies": dependencies } },
            "packages": packages,
        })
        .to_string()
    }

    fn root_inspector(lockfile: impl Into<Vec<u8>>) -> RecordedInspector {
        RecordedInspector::default().with_file(format!("{ROOT}/{BUN_LOCK_FILE_NAME}"), lockfile)
    }

    fn installed(path: &str) -> String {
        format!("{ROOT}/{path}")
    }

    #[test]
    fn test_bun_resolver_identity_names_bun_npm_and_typescript() {
        let resolver = BunResolver::new();
        assert_eq!(resolver.name(), ResolverName::Bun);
        assert_eq!(resolver.manager(), "npm");
        assert_eq!(resolver.language().identity_segment(), "typescript");
        assert_eq!(resolver.manifest_file_name(), "package.json");
    }

    #[test]
    fn test_resolve_fixture_lockfile_catalogs_dev_packages_and_typescript_library() {
        let mut inspector = root_inspector(FIXTURE_LOCKFILE)
            .with_directory(installed("node_modules/typescript/lib"))
            .with_directory(installed("node_modules/typescript-language-server"));

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(
            names(&resolution),
            [
                "npm/typescript@5.9.3",
                "npm/typescript-language-server@6.0.0",
                "stdlib/typescript@5.9.3",
            ]
        );
        let typescript = entry(&resolution, "typescript@5.9.3");
        assert!(
            typescript.is_direct(),
            "a dev dependency is declared directly"
        );
        assert_eq!(typescript.location(), PackageLocation::Dependency);
        assert_eq!(
            typescript.source_root(),
            Some(Path::new("/workspace/node_modules/typescript"))
        );
        let server = entry(&resolution, "typescript-language-server@6.0.0");
        assert!(server.is_direct());
        assert_eq!(
            server.source_root(),
            Some(Path::new(
                "/workspace/node_modules/typescript-language-server"
            ))
        );
        let library = resolution
            .entries
            .iter()
            .find(|entry| entry.location() == PackageLocation::Stdlib)
            .expect("the pinned typescript package gives a library entry");
        assert_eq!(library.identity().manager, "stdlib");
        assert_eq!(
            library.source_root(),
            Some(Path::new("/workspace/node_modules/typescript/lib"))
        );
        assert!(!library.is_direct());
        assert_eq!(
            resolution.inputs,
            [project("package.json"), project("bun.lock")]
        );
        assert!(
            resolution.degradations.is_empty(),
            "{:?}",
            resolution.degradations
        );
    }

    #[test]
    fn test_resolve_nested_key_installs_below_its_parent() {
        let text = lockfile(
            &["parent"],
            &[
                ("parent", "parent@1.0.0"),
                ("parent/child", "child@2.0.0"),
                ("child", "child@1.0.0"),
            ],
        );
        let mut inspector = root_inspector(text)
            .with_directory(installed("node_modules/parent/node_modules/child"))
            .with_directory(installed("node_modules/child"));

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(
            names(&resolution),
            ["npm/child@1.0.0", "npm/child@2.0.0", "npm/parent@1.0.0"]
        );
        let parent = entry(&resolution, "parent@1.0.0");
        assert!(parent.is_direct());
        assert_eq!(
            parent.source_root(),
            Some(Path::new("/workspace/node_modules/parent"))
        );
        let nested = entry(&resolution, "child@2.0.0");
        assert!(!nested.is_direct(), "only the parent declares it");
        assert_eq!(
            nested.source_root(),
            Some(Path::new(
                "/workspace/node_modules/parent/node_modules/child"
            ))
        );
        let hoisted = entry(&resolution, "child@1.0.0");
        assert!(!hoisted.is_direct());
        assert_eq!(
            hoisted.source_root(),
            Some(Path::new("/workspace/node_modules/child"))
        );
    }

    #[test]
    fn test_resolve_scoped_package_keeps_its_scope_in_name_and_path() {
        let text = lockfile(
            &["@scope/pkg"],
            &[
                ("@scope/pkg", "@scope/pkg@1.0.0"),
                ("@types/react", "@types/react@19.2.18"),
                ("@scope/pkg/@types/react", "@types/react@18.0.0"),
            ],
        );
        let mut inspector = root_inspector(text)
            .with_directory(installed(
                "node_modules/@scope/pkg/node_modules/@types/react",
            ))
            .with_directory(installed("node_modules/@types/react"));

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(
            names(&resolution),
            [
                "npm/@scope/pkg@1.0.0",
                "npm/@types/react@18.0.0",
                "npm/@types/react@19.2.18",
            ]
        );
        assert!(entry(&resolution, "@scope/pkg@1.0.0").is_direct());
        assert_eq!(
            entry(&resolution, "@types/react@19.2.18").source_root(),
            Some(Path::new("/workspace/node_modules/@types/react"))
        );
        assert_eq!(
            entry(&resolution, "@types/react@18.0.0").source_root(),
            Some(Path::new(
                "/workspace/node_modules/@scope/pkg/node_modules/@types/react"
            ))
        );
    }

    #[test]
    fn test_resolve_workspace_package_is_skipped() {
        let text = lockfile(
            &["dep", "member"],
            &[
                ("app", "app@workspace:."),
                ("member", "member@workspace:packages/member"),
                ("dep", "dep@1.0.0"),
            ],
        );
        let mut inspector = root_inspector(text);

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(names(&resolution), ["npm/dep@1.0.0"]);
        assert!(resolution.degradations.is_empty());
    }

    #[test]
    fn test_resolve_alias_catalogs_real_name_and_version_at_alias_path() {
        let text = lockfile(
            &["alias"],
            &[
                ("alias", "alias@npm:real@1.2.3"),
                ("@my/alias", "@my/alias@npm:@other/real@2.0.0"),
            ],
        );
        let mut inspector = root_inspector(text)
            .with_directory(installed("node_modules/alias"))
            .with_directory(installed("node_modules/@my/alias"));

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(
            names(&resolution),
            ["npm/@other/real@2.0.0", "npm/real@1.2.3"]
        );
        let real = entry(&resolution, "real@1.2.3");
        assert!(real.is_direct(), "the manifest declares the alias name");
        assert_eq!(
            real.source_root(),
            Some(Path::new("/workspace/node_modules/alias"))
        );
        let scoped = entry(&resolution, "@other/real@2.0.0");
        assert!(!scoped.is_direct());
        assert_eq!(
            scoped.source_root(),
            Some(Path::new("/workspace/node_modules/@my/alias"))
        );
    }

    #[test]
    fn test_resolve_url_version_is_cataloged_verbatim() {
        let text = lockfile(
            &[],
            &[
                ("pkg", "pkg@git+ssh://git@github.com/owner/repo.git#abc123"),
                ("gh", "gh@github:owner/repo#def456"),
                ("local", "local@file:../local"),
            ],
        );
        let mut inspector = root_inspector(text);

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(
            names(&resolution),
            [
                "npm/gh@github:owner/repo#def456",
                "npm/local@file:../local",
                "npm/pkg@git+ssh://git@github.com/owner/repo.git#abc123",
            ]
        );
    }

    #[test]
    fn test_resolve_nested_key_spells_its_path_without_its_parent_pinned() {
        let text = lockfile(&[], &[("orphan/child", "child@1.0.0")]);
        let mut inspector = root_inspector(text)
            .with_directory(installed("node_modules/orphan/node_modules/child"));

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(
            entry(&resolution, "child@1.0.0").source_root(),
            Some(Path::new(
                "/workspace/node_modules/orphan/node_modules/child"
            ))
        );
    }

    #[test]
    fn test_resolve_key_spelling_no_path_gives_no_root_and_no_probe() {
        let text = lockfile(&[], &[("@scope", "@scope@1.0.0")]);
        let mut inspector = root_inspector(text);

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(names(&resolution), ["npm/@scope@1.0.0"]);
        assert_eq!(entry(&resolution, "@scope@1.0.0").source_root(), None);
        assert!(
            !inspector
                .asked
                .iter()
                .any(|asked| asked.starts_with("exists ")),
            "{:?}",
            inspector.asked
        );
    }

    #[test]
    fn test_resolve_source_root_absent_when_directory_missing() {
        let text = lockfile(&["dep"], &[("dep", "dep@1.0.0")]);
        let mut inspector = root_inspector(text);

        let resolution = resolve(&["package.json"], &mut inspector);

        let dep = entry(&resolution, "dep@1.0.0");
        assert_eq!(dep.source_root(), None);
        assert!(dep.is_direct());
        assert!(
            inspector
                .asked
                .contains(&"exists /workspace/node_modules/dep".to_owned()),
            "{:?}",
            inspector.asked
        );
    }

    #[test]
    fn test_resolve_comments_and_trailing_commas_parse() {
        let text = "{\n  // Bun writes JSONC.\n  \"lockfileVersion\": 1, /* block */\n  \
                    \"workspaces\": { \"\": { \"dependencies\": { \"dep\": \"^1.0.0\", }, }, },\n  \
                    \"packages\": {\n    \"dep\": [\"dep@1.0.0\", \"\", {}, \"\",],\n  },\n}\n";
        let mut inspector = root_inspector(text);

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(names(&resolution), ["npm/dep@1.0.0"]);
        assert!(entry(&resolution, "dep@1.0.0").is_direct());
        assert!(resolution.degradations.is_empty());
    }

    #[test]
    fn test_resolve_manifest_without_lockfile_is_silent() {
        let mut inspector = RecordedInspector::default();

        let resolution = resolve(&["package.json"], &mut inspector);

        assert!(resolution.entries.is_empty());
        assert_eq!(resolution.inputs, [project("package.json")]);
        assert!(resolution.degradations.is_empty());
        assert_eq!(inspector.asked, ["read /workspace/bun.lock"]);
    }

    #[test]
    fn test_resolve_nested_manifest_without_lockfile_is_covered_by_root() {
        let text = json!({
            "lockfileVersion": 1,
            "workspaces": {
                "": { "name": "root", "dependencies": { "react": "^19.0.0" } },
                "packages/foo": {
                    "name": "foo",
                    "devDependencies": { "lodash": "^4.0.0" },
                    "optionalDependencies": { "fsevents": "^2.0.0" },
                },
            },
            "packages": {
                "foo": ["foo@workspace:packages/foo", {}],
                "react": ["react@19.2.8", "", {}, ""],
                "lodash": ["lodash@4.17.21", "", {}, ""],
                "fsevents": ["fsevents@2.3.3", "", {}, ""],
            },
        })
        .to_string();
        let mut inspector = root_inspector(text);

        let resolution = resolve(
            &["package.json", "packages/foo/package.json"],
            &mut inspector,
        );

        assert_eq!(
            names(&resolution),
            [
                "npm/fsevents@2.3.3",
                "npm/lodash@4.17.21",
                "npm/react@19.2.8"
            ]
        );
        assert!(
            resolution.entries.iter().all(CatalogEntry::is_direct),
            "every workspace entry's dependency maps count as declared"
        );
        assert_eq!(
            resolution.inputs,
            [
                project("package.json"),
                project("packages/foo/package.json"),
                project("bun.lock"),
            ]
        );
        assert!(resolution.degradations.is_empty());
        assert!(
            inspector
                .asked
                .contains(&"read /workspace/packages/foo/bun.lock".to_owned()),
            "the nested manifest is probed for its own lockfile"
        );
    }

    #[test]
    fn test_resolve_nested_manifest_with_own_lockfile_resolves_separately() {
        let mut inspector = root_inspector(lockfile(&["a"], &[("a", "a@1.0.0")]))
            .with_file(
                installed("tools/x/bun.lock"),
                lockfile(&["b"], &[("b", "b@1.0.0")]),
            )
            .with_directory(installed("node_modules/a"))
            .with_directory(installed("tools/x/node_modules/b"));

        let resolution = resolve(&["package.json", "tools/x/package.json"], &mut inspector);

        assert_eq!(names(&resolution), ["npm/a@1.0.0", "npm/b@1.0.0"]);
        assert_eq!(
            entry(&resolution, "a@1.0.0").source_root(),
            Some(Path::new("/workspace/node_modules/a"))
        );
        assert_eq!(
            entry(&resolution, "b@1.0.0").source_root(),
            Some(Path::new("/workspace/tools/x/node_modules/b"))
        );
        assert_eq!(
            resolution.inputs,
            [
                project("package.json"),
                project("tools/x/package.json"),
                project("bun.lock"),
                project("tools/x/bun.lock"),
            ]
        );
    }

    #[test]
    fn test_resolve_lockfile_over_bound_degrades_without_entries() {
        let oversized = vec![b' '; usize::try_from(LOCKFILE_BYTES_MAX).expect("bound fits") + 1];
        let mut inspector = root_inspector(oversized);

        let resolution = resolve(&["package.json"], &mut inspector);

        assert!(resolution.entries.is_empty());
        assert_eq!(
            resolution.inputs,
            [project("package.json"), project("bun.lock")]
        );
        assert_eq!(
            resolution.degradations,
            [format!(
                "package.json: bun.lock holds {} bytes, past the {LOCKFILE_BYTES_MAX} byte \
                 bound; no packages cataloged",
                LOCKFILE_BYTES_MAX + 1
            )]
        );
    }

    #[test]
    fn test_resolve_lockfile_unparsable_degrades_without_entries() {
        let mut inspector = root_inspector("{ \"lockfileVersion\": ");

        let resolution = resolve(&["package.json"], &mut inspector);

        assert!(resolution.entries.is_empty());
        assert_eq!(resolution.degradations.len(), 1);
        let degradation = &resolution.degradations[0];
        assert!(
            degradation.starts_with("package.json: bun.lock could not be parsed: "),
            "{degradation}"
        );
        assert!(
            degradation.ends_with("; no packages cataloged"),
            "{degradation}"
        );
    }

    #[test]
    fn test_resolve_malformed_tuples_are_counted_not_cataloged() {
        let text = json!({
            "lockfileVersion": 1,
            "packages": {
                "number": [1, "", {}, ""],
                "empty": [],
                "unversioned": ["unversioned@", "", {}, ""],
                "unnamed": ["@1.0.0", "", {}, ""],
                "bare-alias": ["bare-alias@npm:", "", {}, ""],
                "kept": ["kept@1.0.0", "", {}, ""],
            },
        })
        .to_string();
        let mut inspector = root_inspector(text);

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(names(&resolution), ["npm/kept@1.0.0"]);
        assert_eq!(
            resolution.degradations,
            [
                "package.json: 5 bun.lock packages entries open with no `<name>@<version>` \
                 reference; not cataloged"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn test_resolve_newer_lockfile_version_is_read_and_reported() {
        let text = json!({
            "lockfileVersion": LOCKFILE_VERSION_MAX + 1,
            "packages": { "dep": ["dep@1.0.0", "", {}, ""] },
        })
        .to_string();
        let mut inspector = root_inspector(text);

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(names(&resolution), ["npm/dep@1.0.0"]);
        assert_eq!(
            resolution.degradations,
            [format!(
                "package.json: bun.lock states lockfileVersion {}, newer than the \
                 {LOCKFILE_VERSION_MAX} this resolver reads; read as version \
                 {LOCKFILE_VERSION_MAX}",
                LOCKFILE_VERSION_MAX + 1
            )]
        );
    }

    #[test]
    fn test_resolve_typescript_alias_gives_no_library_entry() {
        let text = lockfile(&[], &[("typescript", "typescript@npm:other@1.0.0")]);
        let mut inspector = root_inspector(text);

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(names(&resolution), ["npm/other@1.0.0"]);
    }

    #[test]
    fn test_resolve_packages_over_max_drops_excess_with_degradation() {
        let references: Vec<(String, String)> = (0..=PACKAGES_MAX)
            .map(|index| (format!("pkg-{index:05}"), format!("pkg-{index:05}@1.0.0")))
            .collect();
        let packages: Vec<(&str, &str)> = references
            .iter()
            .map(|(key, reference)| (key.as_str(), reference.as_str()))
            .collect();
        let mut inspector = root_inspector(lockfile(&[], &packages));

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(resolution.entries.len(), PACKAGES_MAX);
        assert!(resolution.entries.iter().all(|entry| !entry.is_direct()));
        assert_eq!(
            resolution.degradations,
            [format!(
                "1 of {} packages were not cataloged: at most {PACKAGES_MAX} are cataloged per \
                 workspace",
                PACKAGES_MAX + 1
            )]
        );
    }

    #[test]
    fn test_resolve_same_inspector_twice_answers_equal_resolutions() {
        let inspector = || {
            root_inspector(FIXTURE_LOCKFILE)
                .with_directory(installed("node_modules/typescript/lib"))
        };

        let first = resolve(&["package.json"], &mut inspector());
        let second = resolve(&["package.json"], &mut inspector());

        assert_eq!(first, second);
        assert_eq!(first.entries.len(), 3);
    }

    #[test]
    fn test_split_reference_splits_past_the_scope_and_keeps_url_versions() {
        assert_eq!(
            split_reference("@types/react@19.2.18"),
            Some(("@types/react", "19.2.18"))
        );
        assert_eq!(
            split_reference("foo@git+ssh://git@host/x"),
            Some(("foo", "git+ssh://git@host/x"))
        );
        assert_eq!(split_reference("@scope@1.0.0"), Some(("@scope", "1.0.0")));
        assert_eq!(split_reference("@1.0.0"), None, "no name");
        assert_eq!(split_reference("foo@"), None, "no version");
        assert_eq!(split_reference("foo"), None, "no separator");
        assert_eq!(split_reference(""), None);
    }

    #[test]
    fn test_pinned_classifies_workspace_alias_and_plain_versions() {
        assert_eq!(pinned("app", "workspace:."), Pin::Workspace);
        assert_eq!(
            pinned("alias", "npm:real@1.2.3"),
            Pin::Package {
                spelled: "alias",
                name: "real",
                version: "1.2.3"
            }
        );
        assert_eq!(pinned("alias", "npm:real"), Pin::Malformed);
        assert_eq!(
            pinned("dep", "1.0.0"),
            Pin::Package {
                spelled: "dep",
                name: "dep",
                version: "1.0.0"
            }
        );
    }

    #[test]
    fn test_nested_names_groups_scopes_and_refuses_empty_segments() {
        assert_eq!(nested_names("a"), Some(vec!["a".to_owned()]));
        assert_eq!(
            nested_names("a/b/c"),
            Some(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()])
        );
        assert_eq!(
            nested_names("@s/n/@t/m"),
            Some(vec!["@s/n".to_owned(), "@t/m".to_owned()])
        );
        assert_eq!(nested_names("@s"), None, "a scope needs a name");
        assert_eq!(nested_names("@s/"), None, "a scope needs a nonempty name");
        assert_eq!(nested_names("a//b"), None, "an empty segment names nothing");
        assert_eq!(nested_names(""), None);
    }

    #[test]
    fn test_install_path_stops_at_the_nesting_bound() {
        let at_bound = vec!["p"; NESTING_DEPTH_MAX].join("/");
        let path = install_path(&at_bound).expect("a key at the bound spells a path");
        assert!(path.starts_with("node_modules/p/node_modules/p/"));
        assert_eq!(path.matches("node_modules").count(), NESTING_DEPTH_MAX);

        let past_bound = vec!["p"; NESTING_DEPTH_MAX + 1].join("/");
        assert_eq!(install_path(&past_bound), None);
    }
}
