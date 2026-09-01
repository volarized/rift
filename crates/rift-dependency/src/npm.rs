//! The npm resolver: npm packages as `package-lock.json` pins them and `node_modules` holds them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rift_protocol::read::{Language, ProjectPath};
use serde::Deserialize;

use crate::catalog::Resolution;
use crate::manifest::{
    LockfileFailure, ResolutionBuilder, file_beside, is_ancestor_directory, manifest_directory,
    manifest_directory_path, read_lockfile,
};
use crate::node::{self, NPM_MANAGER, PACKAGE_MANIFEST_FILE_NAME, TYPESCRIPT_PACKAGE_NAME};
use crate::resolver::{
    DependencyResolver, FileObservation, Inspector, ResolutionRequest, ResolverName,
};

/// The lockfile npm keeps beside the manifest it resolved.
const PACKAGE_LOCK_FILE_NAME: &str = "package-lock.json";
/// The lockfile Bun keeps beside a manifest; where it stands, the Bun resolver answers.
const BUN_LOCK_FILE_NAME: &str = "bun.lock";
/// Bytes a presence probe reads: none, since any answer but absent means the file stands.
const PRESENCE_PROBE_BYTES_MAX: u64 = 0;
/// The `packages` key of the workspace's own root package.
const ROOT_PACKAGE_KEY: &str = "";
/// The first `lockfileVersion` whose document carries the `packages` map.
const PACKAGES_MAP_LOCKFILE_VERSION_MIN: u64 = 2;
/// The directory segment every installed package key carries, with its separator.
const NODE_MODULES_SEGMENT: &str = "node_modules/";
/// The separator that closes the key segment before a `node_modules/` segment.
const KEY_SEPARATOR: char = '/';

/// The resolver for npm packages, answering from `package-lock.json`.
#[derive(Debug, Default)]
pub struct NpmResolver;

impl NpmResolver {
    /// The npm resolver. It holds no state, so one instance serves every workspace.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl DependencyResolver for NpmResolver {
    fn name(&self) -> ResolverName {
        ResolverName::Npm
    }

    fn manager(&self) -> &'static str {
        NPM_MANAGER
    }

    fn language(&self) -> Language {
        node::typescript_language()
    }

    fn manifest_file_name(&self) -> &'static str {
        PACKAGE_MANIFEST_FILE_NAME
    }

    /// Catalogs the packages the request's manifests resolve to.
    ///
    /// A manifest with `package-lock.json` beside it is a lockfile root: the lockfile's
    /// `packages` map is cataloged and the lockfile is an input. A manifest without one
    /// is covered by the nearest listed ancestor manifest that has one, since an npm
    /// workspace keeps one lockfile at its root. A manifest with no lockfile beside it or
    /// above it stays silent when `bun.lock` stands beside it or beside a listed
    /// ancestor, since the Bun resolver answers for it; otherwise it is reported as not
    /// resolved. Every listed manifest is an input. Entries stop at `PACKAGES_MAX` and
    /// the drop is reported. Deciding coverage compares every manifest pair, so that
    /// work is quadratic in the manifest count, which `MANIFESTS_MAX` bounds; each
    /// installed package costs one directory probe, which `LOCKFILE_BYTES_MAX` bounds.
    fn resolve(
        &self,
        request: &ResolutionRequest<'_>,
        inspector: &mut dyn Inspector,
    ) -> Resolution {
        let mut answer = ResolutionBuilder::default();
        let mut lockfile_roots: Vec<&str> = Vec::new();
        let mut uncovered: Vec<&ProjectPath> = Vec::new();
        for manifest in request.manifests {
            answer.input(manifest.clone());
            let directory = manifest_directory_path(request.root, manifest);
            match read_lockfile(&directory, PACKAGE_LOCK_FILE_NAME, inspector) {
                Err(failure) if failure.is_absent() => uncovered.push(manifest),
                observed => {
                    lockfile_roots.push(manifest_directory(manifest));
                    answer.input(file_beside(manifest, PACKAGE_LOCK_FILE_NAME));
                    resolve_lockfile(observed, &directory, manifest, inspector, &mut answer);
                }
            }
        }
        let mut bun_lockfiles = BunLockfiles::default();
        for manifest in uncovered {
            if is_covered(&lockfile_roots, manifest) {
                continue;
            }
            if bun_lockfiles.stand_beside_or_above(request, manifest, inspector) {
                continue;
            }
            answer.degradation(format!(
                "{}: no {PACKAGE_LOCK_FILE_NAME} beside it or above it; not resolved",
                manifest.0
            ));
        }
        answer.build()
    }
}

/// The `package-lock.json` document, the fields this resolver reads.
#[derive(Deserialize)]
struct Lockfile {
    #[serde(rename = "lockfileVersion")]
    lockfile_version: u64,
    #[serde(default)]
    packages: BTreeMap<String, LockedPackage>,
}

/// One entry of the `packages` map, keyed by its install path below the manifest.
///
/// The root package sits at the empty key and its three dependency maps name the
/// packages the workspace declares directly. A `link` entry is a symlink to one of the
/// workspace's own packages and pins no version of its own.
#[derive(Deserialize)]
struct LockedPackage {
    version: Option<String>,
    link: Option<bool>,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "devDependencies")]
    dev_dependencies: BTreeMap<String, String>,
    #[serde(default, rename = "optionalDependencies")]
    optional_dependencies: BTreeMap<String, String>,
}

/// Parses one lockfile document, naming the parser's message when it is not npm's.
fn parse_lockfile(bytes: &[u8]) -> Result<Lockfile, LockfileFailure> {
    serde_json::from_slice(bytes)
        .map_err(|error| LockfileFailure::unparsable(PACKAGE_LOCK_FILE_NAME, error.to_string()))
}

/// Catalogs one lockfile root's packages, or reports why the lockfile answered nothing.
///
/// A lockfile whose `lockfileVersion` predates the `packages` map answers nothing: npm 7
/// or later rewrites it on the next install.
fn resolve_lockfile(
    observed: Result<Vec<u8>, LockfileFailure>,
    manifest_directory: &Path,
    manifest: &ProjectPath,
    inspector: &mut dyn Inspector,
    answer: &mut ResolutionBuilder,
) {
    let manifest_path = &manifest.0;
    match observed.and_then(|bytes| parse_lockfile(&bytes)) {
        Ok(lockfile) if lockfile.lockfile_version < PACKAGES_MAP_LOCKFILE_VERSION_MIN => {
            answer.degradation(format!(
                "{manifest_path}: {PACKAGE_LOCK_FILE_NAME} lockfileVersion {} carries no \
                 packages map; npm 7 or later rewrites it",
                lockfile.lockfile_version
            ));
        }
        Ok(lockfile) => catalog_lockfile(&lockfile, manifest_directory, inspector, answer),
        Err(failure) => {
            answer.degradation(format!("{manifest_path}: {failure}; no packages cataloged"));
        }
    }
}

/// One entry per installed package, plus the TypeScript library when `typescript` is pinned.
fn catalog_lockfile(
    lockfile: &Lockfile,
    manifest_directory: &Path,
    inspector: &mut dyn Inspector,
    answer: &mut ResolutionBuilder,
) {
    let direct = declared_dependencies(lockfile);
    for (key, package) in &lockfile.packages {
        let Some(installed) = installed_package(key, package) else {
            continue;
        };
        answer.entry(node::installed_entry(
            inspector,
            manifest_directory,
            key,
            installed.name,
            installed.version,
            direct.contains(installed.name),
        ));
    }
    if let Some(version) = typescript_version(lockfile) {
        answer.entry(node::typescript_library_entry(
            inspector,
            manifest_directory,
            version,
        ));
    }
}

/// One installed package the lockfile pins: its name and version.
#[derive(Debug, Eq, PartialEq)]
struct InstalledPackage<'a> {
    name: &'a str,
    version: &'a str,
}

/// Classifies one `packages` entry; `None` when it is not an installed package.
///
/// An installed package's key carries a `node_modules/` segment: a hoisted package sits
/// directly under it, a nested one under a further segment, and a workspace package's
/// own install under `<package>/node_modules/`. The name is the text after the last
/// segment, and a scoped name keeps its `@scope/` prefix. A key with no such segment is
/// one of the workspace's own packages, a `link` entry is a symlink to one of them, and
/// an entry without a version pins nothing.
fn installed_package<'a>(key: &'a str, package: &'a LockedPackage) -> Option<InstalledPackage<'a>> {
    let (parent, name) = key.rsplit_once(NODE_MODULES_SEGMENT)?;
    let at_segment = parent.is_empty() || parent.ends_with(KEY_SEPARATOR);
    let linked = package.link == Some(true);
    let named = !name.is_empty();
    match package.version.as_deref() {
        Some(version) if at_segment && !linked && named => Some(InstalledPackage { name, version }),
        _ => None,
    }
}

/// The names the root package declares, across its three dependency maps.
fn declared_dependencies(lockfile: &Lockfile) -> BTreeSet<&str> {
    let Some(root) = lockfile.packages.get(ROOT_PACKAGE_KEY) else {
        return BTreeSet::new();
    };
    root.dependencies
        .keys()
        .chain(root.dev_dependencies.keys())
        .chain(root.optional_dependencies.keys())
        .map(String::as_str)
        .collect()
}

/// The version the hoisted `typescript` install pins, when the lockfile holds one.
fn typescript_version(lockfile: &Lockfile) -> Option<&str> {
    let hoisted = format!("{NODE_MODULES_SEGMENT}{TYPESCRIPT_PACKAGE_NAME}");
    let (key, package) = lockfile.packages.get_key_value(&hoisted)?;
    installed_package(key, package).map(|installed| installed.version)
}

/// The directories probed for `bun.lock`, each probed once however many manifests ask.
#[derive(Default)]
struct BunLockfiles {
    observed: BTreeMap<PathBuf, bool>,
}

impl BunLockfiles {
    /// Whether `bun.lock` stands beside `manifest` or beside any listed ancestor manifest.
    fn stand_beside_or_above(
        &mut self,
        request: &ResolutionRequest<'_>,
        manifest: &ProjectPath,
        inspector: &mut dyn Inspector,
    ) -> bool {
        let directory = manifest_directory(manifest);
        let ancestors = request
            .manifests
            .iter()
            .filter(|other| is_ancestor_directory(manifest_directory(other), directory));
        std::iter::once(manifest).chain(ancestors).any(|candidate| {
            self.stands_beside(&manifest_directory_path(request.root, candidate), inspector)
        })
    }

    /// Whether `bun.lock` stands in `directory`, probing the inspector on first ask.
    fn stands_beside(&mut self, directory: &Path, inspector: &mut dyn Inspector) -> bool {
        *self
            .observed
            .entry(directory.to_path_buf())
            .or_insert_with(|| {
                let path = directory.join(BUN_LOCK_FILE_NAME);
                let observation = inspector.read_file(&path, PRESENCE_PROBE_BYTES_MAX);
                !matches!(observation, FileObservation::Absent)
            })
    }
}

/// Whether a lockfile root's directory is a proper ancestor of the manifest's directory.
fn is_covered(lockfile_roots: &[&str], manifest: &ProjectPath) -> bool {
    let directory = manifest_directory(manifest);
    lockfile_roots
        .iter()
        .any(|root| is_ancestor_directory(root, directory))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::{Map, json};

    use super::*;
    use crate::catalog::{CatalogEntry, PackageLocation};
    use crate::fixture::RecordedInspector;
    use crate::resolver::{LOCKFILE_BYTES_MAX, PACKAGES_MAX};

    const ROOT: &str = "/workspace";

    fn project(path: &str) -> ProjectPath {
        ProjectPath(path.to_owned())
    }

    fn resolve(manifests: &[&str], inspector: &mut RecordedInspector) -> Resolution {
        let manifests: Vec<ProjectPath> = manifests.iter().map(|path| project(path)).collect();
        let request = ResolutionRequest {
            root: Path::new(ROOT),
            manifests: &manifests,
        };
        NpmResolver::new().resolve(&request, inspector)
    }

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

    fn entry<'a>(resolution: &'a Resolution, name: &str) -> &'a CatalogEntry {
        resolution
            .entries
            .iter()
            .find(|entry| entry.identity().name == name)
            .unwrap_or_else(|| panic!("{name} is cataloged"))
    }

    /// A `lockfileVersion` 3 document: root dependencies, a hoisted direct package, a
    /// nested transitive one, a scoped one, a linked workspace package, and `typescript`.
    fn workspace_lockfile() -> String {
        json!({
            "name": "app",
            "version": "1.0.0",
            "lockfileVersion": 3,
            "requires": true,
            "packages": {
                "": {
                    "name": "app",
                    "version": "1.0.0",
                    "workspaces": ["packages/*"],
                    "dependencies": {
                        "@adobe/css-tools": "^4.2.0",
                        "app-lib": "^0.1.0",
                        "chalk": "^4.1.2"
                    },
                    "devDependencies": { "typescript": "^5.6.0" }
                },
                "node_modules/@adobe/css-tools": {
                    "version": "4.2.0",
                    "resolved": "https://registry.npmjs.org/@adobe/css-tools/-/css-tools-4.2.0.tgz",
                    "dev": true
                },
                "node_modules/@babel/highlight": {
                    "version": "7.24.0",
                    "dependencies": { "ansi-styles": "^3.2.1" }
                },
                "node_modules/@babel/highlight/node_modules/ansi-styles": {
                    "version": "3.2.1"
                },
                "node_modules/ansi-styles": { "version": "4.3.0" },
                "node_modules/app-lib": { "resolved": "packages/app-lib", "link": true },
                "node_modules/chalk": {
                    "version": "4.1.2",
                    "dependencies": { "ansi-styles": "^4.1.0" }
                },
                "node_modules/typescript": { "version": "5.6.3", "dev": true },
                "packages/app-lib": { "name": "app-lib", "version": "0.1.0" }
            }
        })
        .to_string()
    }

    fn workspace_inspector() -> RecordedInspector {
        RecordedInspector::default()
            .with_file(format!("{ROOT}/package-lock.json"), workspace_lockfile())
            .with_directory(format!("{ROOT}/node_modules/chalk"))
            .with_directory(format!(
                "{ROOT}/node_modules/@babel/highlight/node_modules/ansi-styles"
            ))
            .with_directory(format!("{ROOT}/node_modules/typescript/lib"))
    }

    fn lockfile_reads(inspector: &RecordedInspector) -> Vec<&String> {
        inspector
            .asked
            .iter()
            .filter(|line| line.ends_with(PACKAGE_LOCK_FILE_NAME))
            .collect()
    }

    #[test]
    fn test_npm_resolver_identity_names_npm_and_typescript() {
        let resolver = NpmResolver::new();
        assert_eq!(resolver.name(), ResolverName::Npm);
        assert_eq!(resolver.manager(), "npm");
        assert_eq!(resolver.language().identity_segment(), "typescript");
        assert_eq!(resolver.manifest_file_name(), "package.json");
    }

    #[test]
    fn test_resolve_lockfile_catalogs_installed_packages() {
        let mut inspector = workspace_inspector();

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(
            names(&resolution),
            [
                "npm/@adobe/css-tools@4.2.0",
                "npm/@babel/highlight@7.24.0",
                "npm/ansi-styles@3.2.1",
                "npm/ansi-styles@4.3.0",
                "npm/chalk@4.1.2",
                "npm/typescript@5.6.3",
                "stdlib/typescript@5.6.3",
            ],
            "the linked workspace package and its own key are skipped"
        );
        let chalk = entry(&resolution, "chalk");
        assert!(chalk.is_direct(), "the root package depends on chalk");
        assert_eq!(chalk.location(), PackageLocation::Dependency);
        assert_eq!(
            chalk.source_root(),
            Some(Path::new("/workspace/node_modules/chalk"))
        );
        let scoped = entry(&resolution, "@adobe/css-tools");
        assert!(scoped.is_direct());
        assert_eq!(scoped.source_root(), None, "not installed, no root");
        let highlight = entry(&resolution, "@babel/highlight");
        assert!(!highlight.is_direct(), "only the lockfile's graph names it");
        let nested = &resolution.entries[2];
        assert_eq!(nested.identity().version, "3.2.1");
        assert!(!nested.is_direct());
        assert_eq!(
            nested.source_root(),
            Some(Path::new(
                "/workspace/node_modules/@babel/highlight/node_modules/ansi-styles"
            )),
            "a nested install roots at the lockfile's own key"
        );
        let hoisted = &resolution.entries[3];
        assert_eq!(hoisted.identity().version, "4.3.0");
        assert_eq!(hoisted.source_root(), None);
        let typescript = entry(&resolution, "typescript");
        assert!(typescript.is_direct(), "devDependencies count as declared");
        let library = &resolution.entries[6];
        assert_eq!(library.location(), PackageLocation::Stdlib);
        assert_eq!(
            library.source_root(),
            Some(Path::new("/workspace/node_modules/typescript/lib"))
        );
        assert_eq!(library.language().identity_segment(), "typescript");
        assert_eq!(
            resolution.inputs,
            [project("package.json"), project("package-lock.json")]
        );
        assert!(
            resolution.degradations.is_empty(),
            "{:?}",
            resolution.degradations
        );
        assert_eq!(lockfile_reads(&inspector).len(), 1);
    }

    #[test]
    fn test_resolve_typescript_pinned_without_lib_directory_has_no_root() {
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/package-lock.json"), workspace_lockfile());

        let resolution = resolve(&["package.json"], &mut inspector);

        let library = &resolution.entries[6];
        assert_eq!(library.identity().manager, "stdlib");
        assert_eq!(library.identity().version, "5.6.3");
        assert_eq!(library.source_root(), None);
        assert!(
            inspector
                .asked
                .contains(&"exists /workspace/node_modules/typescript/lib".to_owned())
        );
    }

    #[test]
    fn test_resolve_lockfile_version_1_degrades_without_entries() {
        let legacy = json!({
            "name": "app",
            "lockfileVersion": 1,
            "dependencies": { "chalk": { "version": "4.1.2" } }
        })
        .to_string();
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/package-lock.json"), legacy);

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(
            resolution.degradations,
            [
                "package.json: package-lock.json lockfileVersion 1 carries no packages map; npm 7 \
                 or later rewrites it"
            ]
        );
        assert!(resolution.entries.is_empty());
        assert_eq!(
            resolution.inputs,
            [project("package.json"), project("package-lock.json")]
        );
    }

    #[test]
    fn test_resolve_lockfile_absent_with_bun_lock_stays_silent() {
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/bun.lock"), "{}");

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(
            resolution,
            Resolution {
                entries: Vec::new(),
                inputs: vec![project("package.json")],
                degradations: Vec::new(),
            }
        );
        assert_eq!(
            inspector.asked,
            [
                "read /workspace/package-lock.json",
                "read /workspace/bun.lock"
            ]
        );
    }

    #[test]
    fn test_resolve_lockfile_absent_without_bun_lock_degrades() {
        let mut inspector = RecordedInspector::default().with_directory(ROOT);

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(
            resolution.degradations,
            ["package.json: no package-lock.json beside it or above it; not resolved"]
        );
        assert!(resolution.entries.is_empty());
        assert_eq!(resolution.inputs, [project("package.json")]);
    }

    #[test]
    fn test_resolve_nested_manifest_without_lockfile_is_covered_by_root() {
        let mut inspector = workspace_inspector();

        let resolution = resolve(
            &["package.json", "packages/app-lib/package.json"],
            &mut inspector,
        );

        assert_eq!(
            lockfile_reads(&inspector),
            [
                "read /workspace/package-lock.json",
                "read /workspace/packages/app-lib/package-lock.json",
            ],
            "the nested lockfile is probed once and found absent"
        );
        assert_eq!(
            resolution.inputs,
            [
                project("package.json"),
                project("package-lock.json"),
                project("packages/app-lib/package.json"),
            ]
        );
        assert!(
            resolution.degradations.is_empty(),
            "{:?}",
            resolution.degradations
        );
        assert!(
            !inspector
                .asked
                .iter()
                .any(|line| line.ends_with("bun.lock")),
            "a covered manifest never probes for bun.lock"
        );
        assert_eq!(resolution.entries.len(), 7);
    }

    #[test]
    fn test_resolve_nested_manifest_with_bun_lock_above_stays_silent() {
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/bun.lock"), "{}");

        let resolution = resolve(
            &[
                "package.json",
                "packages/app-lib/package.json",
                "packages/tool/package.json",
            ],
            &mut inspector,
        );

        assert!(
            resolution.degradations.is_empty(),
            "{:?}",
            resolution.degradations
        );
        let bun_probes: Vec<&String> = inspector
            .asked
            .iter()
            .filter(|line| line.ends_with("bun.lock"))
            .collect();
        assert_eq!(
            bun_probes,
            [
                "read /workspace/bun.lock",
                "read /workspace/packages/app-lib/bun.lock",
                "read /workspace/packages/tool/bun.lock",
            ],
            "the root is probed once for every manifest below it"
        );
    }

    #[test]
    fn test_resolve_nested_manifest_with_own_lockfile_resolves_separately() {
        let nested = json!({
            "lockfileVersion": 3,
            "packages": {
                "": { "dependencies": { "left-pad": "^1.3.0" } },
                "node_modules/left-pad": { "version": "1.3.0" }
            }
        })
        .to_string();
        let mut inspector = workspace_inspector()
            .with_file(format!("{ROOT}/packages/tool/package-lock.json"), nested)
            .with_directory(format!("{ROOT}/packages/tool/node_modules/left-pad"));

        let resolution = resolve(
            &["package.json", "packages/tool/package.json"],
            &mut inspector,
        );

        assert_eq!(lockfile_reads(&inspector).len(), 2);
        assert_eq!(
            resolution.inputs,
            [
                project("package.json"),
                project("package-lock.json"),
                project("packages/tool/package.json"),
                project("packages/tool/package-lock.json"),
            ]
        );
        let left_pad = entry(&resolution, "left-pad");
        assert!(left_pad.is_direct());
        assert_eq!(
            left_pad.source_root(),
            Some(Path::new("/workspace/packages/tool/node_modules/left-pad"))
        );
        assert_eq!(resolution.entries.len(), 8);
        assert!(resolution.degradations.is_empty());
    }

    #[test]
    fn test_resolve_workspace_nested_install_is_cataloged_at_its_key() {
        let lockfile = json!({
            "lockfileVersion": 3,
            "packages": {
                "": { "workspaces": ["packages/*"] },
                "node_modules/app": { "resolved": "packages/app", "link": true },
                "packages/app": {
                    "name": "app",
                    "version": "0.1.0",
                    "dependencies": { "left-pad": "^1.3.0" }
                },
                "packages/app/node_modules/left-pad": { "version": "1.3.0" }
            }
        })
        .to_string();
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/package-lock.json"), lockfile)
            .with_directory(format!("{ROOT}/packages/app/node_modules/left-pad"));

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(
            names(&resolution),
            ["npm/left-pad@1.3.0"],
            "the workspace package and its link are skipped"
        );
        let left_pad = entry(&resolution, "left-pad");
        assert!(
            !left_pad.is_direct(),
            "only the root package's dependency maps count as declared"
        );
        assert_eq!(
            left_pad.source_root(),
            Some(Path::new("/workspace/packages/app/node_modules/left-pad"))
        );
        assert!(resolution.degradations.is_empty());
    }

    #[test]
    fn test_resolve_lockfile_over_bound_degrades_without_entries() {
        let oversized = vec![b'{'; usize::try_from(LOCKFILE_BYTES_MAX).expect("bound fits") + 1];
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/package-lock.json"), oversized);

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(
            resolution.degradations,
            [format!(
                "package.json: package-lock.json holds {} bytes, past the {LOCKFILE_BYTES_MAX} \
                 byte bound; no packages cataloged",
                LOCKFILE_BYTES_MAX + 1
            )]
        );
        assert!(resolution.entries.is_empty());
        assert_eq!(
            resolution.inputs,
            [project("package.json"), project("package-lock.json")]
        );
    }

    #[test]
    fn test_resolve_lockfile_unparsable_degrades_without_entries() {
        let mut inspector = RecordedInspector::default().with_file(
            format!("{ROOT}/package-lock.json"),
            "{\"lockfileVersion\": ",
        );

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(resolution.degradations.len(), 1);
        let degradation = &resolution.degradations[0];
        assert!(
            degradation.starts_with("package.json: package-lock.json could not be parsed: "),
            "{degradation}"
        );
        assert!(
            degradation.ends_with("; no packages cataloged"),
            "{degradation}"
        );
        assert!(resolution.entries.is_empty());
    }

    #[test]
    fn test_resolve_packages_over_max_drops_excess_with_degradation() {
        let mut packages = Map::new();
        packages.insert(ROOT_PACKAGE_KEY.to_owned(), json!({ "name": "app" }));
        for index in 0..=PACKAGES_MAX {
            let key = format!("node_modules/pkg-{index:05}");
            packages.insert(key, json!({ "version": "1.0.0" }));
        }
        let lockfile = json!({ "lockfileVersion": 3, "packages": packages }).to_string();
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/package-lock.json"), lockfile);

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
        let first = resolve(&["package.json"], &mut workspace_inspector());
        let second = resolve(&["package.json"], &mut workspace_inspector());

        assert_eq!(first, second);
        assert_eq!(first.entries.len(), 7);
    }

    #[test]
    fn test_installed_package_names_the_text_after_the_last_node_modules_segment() {
        let pinned = LockedPackage {
            version: Some("1.0.0".to_owned()),
            link: None,
            dependencies: BTreeMap::new(),
            dev_dependencies: BTreeMap::new(),
            optional_dependencies: BTreeMap::new(),
        };
        let expected = |name| {
            Some(InstalledPackage {
                name,
                version: "1.0.0",
            })
        };
        assert_eq!(
            installed_package("node_modules/@adobe/css-tools", &pinned),
            expected("@adobe/css-tools")
        );
        assert_eq!(
            installed_package(
                "node_modules/@babel/highlight/node_modules/ansi-styles",
                &pinned
            ),
            expected("ansi-styles")
        );
        assert_eq!(installed_package("packages/app", &pinned), None);
        assert_eq!(
            installed_package("packages/app/node_modules/x", &pinned),
            expected("x"),
            "an install below a workspace package is cataloged"
        );
        assert_eq!(
            installed_package("my_node_modules/x", &pinned),
            None,
            "the segment is a whole path segment"
        );
        assert_eq!(
            installed_package("node_modules/", &pinned),
            None,
            "an empty name pins nothing"
        );
        let linked = LockedPackage {
            link: Some(true),
            ..pinned
        };
        assert_eq!(installed_package("node_modules/app-lib", &linked), None);
        let unversioned = LockedPackage {
            version: None,
            ..linked
        };
        assert_eq!(
            installed_package("node_modules/app-lib", &unversioned),
            None
        );
    }

    #[test]
    fn test_resolve_lockfile_without_a_root_entry_catalogs_nothing_as_direct() {
        let rootless = json!({
            "name": "app",
            "lockfileVersion": 3,
            "packages": {
                "node_modules/chalk": { "version": "4.1.2" }
            }
        })
        .to_string();
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/package-lock.json"), rootless);

        let resolution = resolve(&["package.json"], &mut inspector);

        assert_eq!(names(&resolution), ["npm/chalk@4.1.2"]);
        assert!(
            resolution.entries.iter().all(|entry| !entry.is_direct()),
            "no root entry declares anything directly"
        );
        assert!(resolution.degradations.is_empty());
    }
}\n