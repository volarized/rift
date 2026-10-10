//! The Cargo static context: the packages `Cargo.lock` pins and `Cargo.toml` declares.

use std::collections::BTreeMap;
use std::path::Path;

use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry, PackageSelector};
use rift_protocol::read::{PackageIdentity, ProjectPath};
use serde::Deserialize;

use super::lockfile::{GIT_SOURCE_PREFIX, LockedPackage, parse_lockfile};
use super::{CARGO_LOCK_FILE_NAME, CARGO_MANAGER, CARGO_MANIFEST_FILE_NAME};
use crate::context::{ContextAnswer, InstallFolder, InstallLocation, is_whole_version};
use crate::manifest::{
    StaticFileFailure, WorkspacePaths, file_beside, manifest_directory_path, read_static_file,
    top_level_manifests,
};
use crate::resolver::{ContextRequest, StaticInputs};

/// The lockfile `source` values naming crates.io, trailing separator dropped: the git
/// index and the sparse index. Every other registry is a private one.
const CRATES_IO_SOURCES: [&str; 2] = [
    "registry+https://github.com/rust-lang/crates.io-index",
    "sparse+https://index.crates.io",
];
/// The folder below the standard library source that holds the packages it vendors.
const VENDOR_DIRECTORY_NAME: &str = "vendor";
/// The operator that pins one exact version in a Cargo requirement.
const EXACT_OPERATOR: char = '=';
/// The separator between the clauses of a multi-clause Cargo requirement.
const CLAUSE_SEPARATOR: char = ',';

/// Reports what one workspace's `Cargo.lock` files pin and its `Cargo.toml` files
/// declare.
///
/// Every top-level manifest's lockfile contributes the exact version it pins, and every
/// listed manifest the requirements it declares. Each registry package the lockfile pins
/// records its install folder, the `<name>-<version>` folder Cargo unpacks it into. Each
/// manifest and each top-level lockfile is an input. A requirement for a package some lockfile pins is dropped where
/// the answers merge, so the context reports one selector per package. An absent file is
/// the manifest-only case, not a degradation; a file over its bound or unparsable is
/// one, naming the path.
pub(super) fn cargo_context(
    request: &ContextRequest<'_>,
    inputs: &mut dyn StaticInputs,
) -> ContextAnswer {
    let mut answer = ContextAnswer::default();
    let mut parsed = Vec::with_capacity(request.manifests.len());
    for manifest in request.manifests {
        answer.inputs.push(manifest.clone());
        match read_manifest(request.root, manifest, inputs) {
            Ok(Some(document)) => {
                parsed.push((manifest_directory_path(request.root, manifest), document));
            }
            Ok(None) => {}
            Err(failure) => report(&mut answer, manifest, &failure),
        }
    }
    let workspace = WorkspacePaths::new(request.root, inputs);
    let mut declared: BTreeMap<String, Declaration> = BTreeMap::new();
    for (directory, document) in &parsed {
        for (name, dependency) in document.named() {
            let found = declaration(&workspace, directory, dependency, inputs);
            declared
                .entry(name.to_owned())
                .and_modify(|standing| *standing = standing.joined(found))
                .or_insert(found);
        }
    }
    for manifest in top_level_manifests(request.manifests) {
        answer
            .inputs
            .push(file_beside(manifest, CARGO_LOCK_FILE_NAME));
        pin_lockfile(request.root, manifest, inputs, &declared, &mut answer);
    }
    for (directory, document) in &parsed {
        answer.entries.extend(
            document.declared(|dependency| declaration(&workspace, directory, dependency, inputs)),
        );
    }
    answer
}

/// How the manifests declare one package: by a path inside the workspace root alone,
/// which the local index covers as project source, or otherwise.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Declaration {
    /// Every path declaration resolves inside the root; no declaration names a source.
    Inside,
    /// A path outside the root, or a declaration naming no path.
    Elsewhere,
    /// Inherited from the workspace table, `{ workspace = true }`: says nothing alone.
    Inherited,
}

impl Declaration {
    const fn joined(self, other: Self) -> Self {
        match (self, other) {
            (Self::Elsewhere, _) | (_, Self::Elsewhere) => Self::Elsewhere,
            (Self::Inside, _) | (_, Self::Inside) => Self::Inside,
            (Self::Inherited, Self::Inherited) => Self::Inherited,
        }
    }
}

/// How one dependency table value declares its package, from the manifest in
/// `directory`.
fn declaration(
    workspace: &WorkspacePaths,
    directory: &Path,
    dependency: &Dependency,
    inputs: &mut dyn StaticInputs,
) -> Declaration {
    let Dependency::Detailed(detailed) = dependency else {
        return Declaration::Elsewhere;
    };
    let Some(path) = detailed.path.as_deref() else {
        return if detailed.workspace == Some(true) {
            Declaration::Inherited
        } else {
            Declaration::Elsewhere
        };
    };
    if workspace.contains(&directory.join(path), inputs) {
        Declaration::Inside
    } else {
        Declaration::Elsewhere
    }
}

/// Reports every package the `Cargo.lock` beside one manifest pins.
fn pin_lockfile(
    root: &Path,
    manifest: &ProjectPath,
    inputs: &mut dyn StaticInputs,
    declared: &BTreeMap<String, Declaration>,
    answer: &mut ContextAnswer,
) {
    let directory = manifest_directory_path(root, manifest);
    let observed = read_static_file(&directory, CARGO_LOCK_FILE_NAME, inputs);
    let lockfile = match observed.and_then(|bytes| parse_lockfile(&bytes)) {
        Ok(lockfile) => lockfile,
        Err(failure) => return report(answer, manifest, &failure),
    };
    for package in &lockfile.package {
        let Some(mut availability) = source_availability(
            package.source.as_deref(),
            declared.get(&package.name).copied(),
        ) else {
            continue;
        };
        let registry = source_registry(package.source.as_deref());
        if matches!(
            availability,
            PackageAvailability::Canonical | PackageAvailability::PrivateRegistry
        ) && registry.is_none()
        {
            answer
                .degradations
                .push("package registry endpoint is unresolved".to_owned());
            availability = PackageAvailability::RegistryUnresolved;
        }
        if let Some(folder) = registry_folder(package, availability) {
            answer.install_folders.push(folder);
        }
        let mut entry = PackageContextEntry::new(
            CARGO_MANAGER,
            &package.name,
            PackageSelector::Version(package.version.clone()),
            availability,
        );
        entry.registry = registry;
        answer.entries.push(entry);
    }
}

/// The folder a registry package unpacks into, below each registry source folder.
fn registry_folder(
    package: &LockedPackage,
    availability: PackageAvailability,
) -> Option<InstallFolder> {
    let (package, folder) = registry_package(package, availability)?;
    Some(InstallFolder {
        origin: rift_protocol::read::SourceLocation::Dependency { package },
        location: InstallLocation::CargoRegistry(folder),
    })
}

/// A registry package's identity and the folder Cargo unpacks it into, `<name>-<version>`,
/// minted from the lockfile's own name and version: a folder name is never parsed, since
/// `md-5-0.11.0` and `toml-0.9.12+spec-1.1.0` split no way that holds. Absent for a git or
/// path package, whose checkout folder Cargo names by a hash this pass cannot mint.
fn registry_package(
    package: &LockedPackage,
    availability: PackageAvailability,
) -> Option<(PackageIdentity, String)> {
    let registry = matches!(
        availability,
        PackageAvailability::Canonical | PackageAvailability::PrivateRegistry
    );
    registry
        .then(|| {
            let registry = source_registry(package.source.as_deref())?;
            let identity = PackageIdentity {
                manager: CARGO_MANAGER.to_owned(),
                registry,
                name: package.name.clone(),
                version: package.version.clone(),
            };
            identity.owner().ok()?;
            Some((identity, format!("{}-{}", package.name, package.version)))
        })
        .flatten()
}

/// The defining registry the Cargo lockfile names, after accepted source normalization.
fn source_registry(source: Option<&str>) -> Option<String> {
    let source = source?.trim_end_matches('/');
    if CRATES_IO_SOURCES.contains(&source) {
        return Some("crates.io".to_owned());
    }
    let endpoint = source
        .strip_prefix("registry+")
        .or_else(|| source.strip_prefix("sparse+"))?;
    rift_protocol::identity::canonical_registry_endpoint(endpoint).ok()
}

/// The registry packages the Rust standard library source vendors, each in its
/// `vendor/<name>-<version>` folder below `library`.
///
/// The `rust-src` component vendors every registry package the standard library's own
/// `Cargo.lock` pins, under the folder name Cargo unpacks it into, so each folder is minted
/// from the lockfile as a registry folder is and no folder name is read. A package without
/// a registry source, such as `std` itself, has no folder there. An absent lockfile, as on
/// a toolchain without `rust-src`, names none.
///
/// # Errors
///
/// Returns `StaticFileFailure` when the lockfile is over its bound or unparsable.
pub(crate) fn vendored_folders(
    library: &Path,
    inputs: &mut dyn StaticInputs,
) -> Result<Vec<InstallFolder>, StaticFileFailure> {
    let lockfile = match read_static_file(library, CARGO_LOCK_FILE_NAME, inputs) {
        Ok(bytes) => parse_lockfile(&bytes)?,
        Err(failure) if failure.is_absent() => return Ok(Vec::new()),
        Err(failure) => return Err(failure),
    };
    let vendor = library.join(VENDOR_DIRECTORY_NAME);
    Ok(lockfile
        .package
        .iter()
        .filter_map(|package| {
            let availability = source_availability(package.source.as_deref(), None)?;
            let (package, folder) = registry_package(package, availability)?;
            Some(InstallFolder {
                origin: rift_protocol::read::SourceLocation::Dependency { package },
                location: InstallLocation::Path(vendor.join(folder)),
            })
        })
        .collect())
}

/// One `Cargo.toml`'s parsed dependency tables, absent when no file stands there.
fn read_manifest(
    root: &Path,
    manifest: &ProjectPath,
    inputs: &mut dyn StaticInputs,
) -> Result<Option<Manifest>, StaticFileFailure> {
    let directory = manifest_directory_path(root, manifest);
    match read_static_file(&directory, CARGO_MANIFEST_FILE_NAME, inputs) {
        Ok(bytes) => parse_manifest(&bytes).map(Some),
        Err(failure) if failure.is_absent() => Ok(None),
        Err(failure) => Err(failure),
    }
}

/// Records one unreadable input, naming the path. An absent file is the manifest-only
/// case and states nothing to report.
fn report(answer: &mut ContextAnswer, manifest: &ProjectPath, failure: &StaticFileFailure) {
    if failure.is_absent() {
        return;
    }
    let manifest_path = &manifest.0;
    answer
        .degradations
        .push(format!("{manifest_path}: {failure}; no packages reported"));
}

/// Whether a global package index can answer for a locked package's `source`, and
/// whether the package is reported at all.
///
/// A package with no `source` is one of the workspace's own or a path dependency, and
/// the lockfile does not tell the two apart. A manifest's dependency table does: a path
/// dependency outside the root is declared there and is reported as a path entry, while
/// the workspace's own package, declared by nobody or by a path inside the root, is
/// project source and is reported by nobody. A `git+` source is a git entry, and every
/// registry but crates.io a private one.
fn source_availability(
    source: Option<&str>,
    declaration: Option<Declaration>,
) -> Option<PackageAvailability> {
    let Some(source) = source.map(|source| source.trim_end_matches('/')) else {
        return (declaration == Some(Declaration::Elsewhere)).then_some(PackageAvailability::Path);
    };
    if CRATES_IO_SOURCES.contains(&source) {
        return Some(PackageAvailability::Canonical);
    }
    if source.starts_with(GIT_SOURCE_PREFIX) {
        return Some(PackageAvailability::Git);
    }
    Some(if source_registry(Some(source)).is_some() {
        PackageAvailability::PrivateRegistry
    } else {
        PackageAvailability::RegistryUnresolved
    })
}

/// The `Cargo.toml` document, the dependency tables this pass reads.
///
/// A target-conditional table is not read: its key carries a `cfg` expression this pass
/// does not evaluate.
#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    dependencies: BTreeMap<String, Dependency>,
    #[serde(default, rename = "dev-dependencies")]
    dev_dependencies: BTreeMap<String, Dependency>,
    #[serde(default, rename = "build-dependencies")]
    build_dependencies: BTreeMap<String, Dependency>,
    workspace: Option<ManifestWorkspace>,
}

/// The `[workspace]` table, the one key this pass reads.
#[derive(Deserialize)]
struct ManifestWorkspace {
    #[serde(default)]
    dependencies: BTreeMap<String, Dependency>,
}

impl Manifest {
    /// Every package the dependency tables name, whatever selector each states, with
    /// the value naming it.
    ///
    /// A lockfile package with no `source` is reported only when one of these names it
    /// by a path outside the root: that is what separates a path dependency from the
    /// workspace's own package.
    fn named(&self) -> impl Iterator<Item = (&str, &Dependency)> {
        self.tables()
            .flat_map(|table| table.iter())
            .map(|(key, dependency)| (dependency.package_name(key), dependency))
    }

    /// Every dependency table this pass reads, in the order it reads them.
    fn tables(&self) -> impl Iterator<Item = &BTreeMap<String, Dependency>> {
        [
            &self.dependencies,
            &self.dev_dependencies,
            &self.build_dependencies,
        ]
        .into_iter()
        .chain(
            self.workspace
                .iter()
                .map(|workspace| &workspace.dependencies),
        )
    }

    /// One entry per dependency table value stating a version, in table then key order.
    /// A value whose path lies inside the root is project source and states none.
    fn declared<'a>(
        &'a self,
        mut declaration: impl FnMut(&Dependency) -> Declaration + 'a,
    ) -> impl Iterator<Item = PackageContextEntry> + 'a {
        self.tables()
            .flat_map(|table| table.iter())
            .filter(move |(_, dependency)| declaration(dependency) != Declaration::Inside)
            .filter_map(|(key, dependency)| dependency.entry(key))
    }
}

/// One dependency table value: a bare requirement, or a table stating where the package
/// comes from.
#[derive(Deserialize)]
#[serde(untagged)]
enum Dependency {
    /// `serde = "1.0"`: the requirement alone, from the default registry.
    Requirement(String),
    /// `serde = { version = "1.0", features = [...] }` and every other table form.
    Detailed(DetailedDependency),
}

/// The table form of a dependency, the keys this pass reads.
///
/// A table with no `version` states no selector: `{ workspace = true }` inherits the
/// requirement the workspace table declares, and `{ path = "../tool" }` names a
/// directory. Neither is reported.
#[derive(Deserialize)]
struct DetailedDependency {
    version: Option<String>,
    path: Option<String>,
    git: Option<String>,
    registry: Option<String>,
    #[serde(rename = "registry-index")]
    registry_index: Option<String>,
    package: Option<String>,
    workspace: Option<bool>,
}

impl Dependency {
    /// The entry this value declares, absent when it states no version.
    fn entry(&self, key: &str) -> Option<PackageContextEntry> {
        let (name, requirement, availability) = match self {
            Self::Requirement(requirement) => {
                (key, requirement.as_str(), PackageAvailability::Canonical)
            }
            Self::Detailed(detailed) => (
                detailed.package.as_deref().unwrap_or(key),
                detailed.version.as_deref()?,
                detailed.availability(),
            ),
        };
        Some(PackageContextEntry::new(
            CARGO_MANAGER,
            name,
            selector(requirement),
            availability,
        ))
    }
}

impl Dependency {
    /// The package this value names: the `package` rename where one is stated, else the
    /// table key.
    fn package_name<'value>(&'value self, key: &'value str) -> &'value str {
        match self {
            Self::Requirement(_) => key,
            Self::Detailed(detailed) => detailed.package.as_deref().unwrap_or(key),
        }
    }
}

impl DetailedDependency {
    /// Whether a global package index can answer for this declaration. A `path` decides
    /// first, since Cargo builds from it whatever else the table states; then `git`, then
    /// a `registry` or `registry-index` other than crates.io.
    fn availability(&self) -> PackageAvailability {
        if self.path.is_some() {
            PackageAvailability::Path
        } else if self.git.is_some() {
            PackageAvailability::Git
        } else if self.registry.is_some() || self.registry_index.is_some() {
            PackageAvailability::PrivateRegistry
        } else {
            PackageAvailability::Canonical
        }
    }
}

/// Parses `Cargo.toml` bytes, naming the parser's message when they are not its document.
fn parse_manifest(bytes: &[u8]) -> Result<Manifest, StaticFileFailure> {
    toml::from_slice(bytes).map_err(|error| {
        StaticFileFailure::unparsable(CARGO_MANIFEST_FILE_NAME, error.message().to_owned())
    })
}

/// The selector one Cargo requirement states.
fn selector(requirement: &str) -> PackageSelector {
    match pinned_version(requirement) {
        Some(version) => PackageSelector::Version(version.to_owned()),
        None => PackageSelector::Requirement(requirement.to_owned()),
    }
}

/// The one exact version a Cargo requirement pins, absent when it admits a range.
///
/// Cargo reads a bare `1.2.3` as the caret requirement `^1.2.3`, so only `=` over a
/// whole version names one release; a requirement of several clauses names a range
/// whatever its clauses say.
fn pinned_version(requirement: &str) -> Option<&str> {
    let stated = requirement.trim();
    if stated.contains(CLAUSE_SEPARATOR) {
        return None;
    }
    let exact = stated.strip_prefix(EXACT_OPERATOR)?.trim_start();
    is_whole_version(exact).then_some(exact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CargoResolver;
    use crate::fixture::RecordedInspector;
    use crate::resolver::{DependencyResolver, LOCKFILE_BYTES_MAX};

    const ROOT: &str = "/workspace";

    fn project(path: &str) -> ProjectPath {
        ProjectPath(path.to_owned())
    }

    /// A lockfile pinning one crates.io package, one custom-registry package, and one
    /// git package, beside the workspace's own member.
    const LOCKFILE: &str = "\
version = 4

[[package]]
name = \"workspace-member\"
version = \"0.1.0\"

[[package]]
name = \"serde\"
version = \"1.0.228\"
source = \"registry+https://github.com/rust-lang/crates.io-index\"

[[package]]
name = \"internal-tool\"
version = \"2.0.0\"
source = \"registry+https://example.test/index\"

[[package]]
name = \"ty_project\"
version = \"0.0.0\"
source = \"git+https://github.com/astral-sh/ruff?rev=2b0d21#2b0d210\"
";

    fn context(manifests: &[&str], inspector: &mut RecordedInspector) -> ContextAnswer {
        let manifests: Vec<ProjectPath> = manifests.iter().map(|path| project(path)).collect();
        let request = ContextRequest {
            root: Path::new(ROOT),
            manifests: &manifests,
        };
        CargoResolver::new().context(&request, inspector)
    }

    fn spelled(answer: &ContextAnswer) -> Vec<String> {
        answer
            .entries
            .iter()
            .map(|entry| {
                let selector = entry.version.as_deref().map_or_else(
                    || {
                        format!(
                            "requirement {}",
                            entry.requirement.clone().unwrap_or_default()
                        )
                    },
                    |version| format!("version {version}"),
                );
                format!("{}: {selector}", entry.name)
            })
            .collect()
    }

    #[test]
    fn test_a_lockfile_pins_exact_versions_and_sorts_every_other_source_by_kind() {
        // No manifest declares `workspace-member`, so its source-less entry is the
        // workspace's own package rather than a path dependency.
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/Cargo.lock"), LOCKFILE);

        let answer = context(&["Cargo.toml"], &mut inspector);

        assert_eq!(
            spelled(&answer),
            [
                "serde: version 1.0.228",
                "internal-tool: version 2.0.0",
                "ty_project: version 0.0.0"
            ],
            "a source-less package no manifest declares is the workspace's own"
        );
        let availability: Vec<PackageAvailability> = answer
            .entries
            .iter()
            .map(|entry| entry.availability)
            .collect();
        assert_eq!(
            availability,
            [
                PackageAvailability::Canonical,
                PackageAvailability::PrivateRegistry,
                PackageAvailability::Git
            ]
        );
        assert!(
            answer
                .entries
                .iter()
                .all(|entry| entry.violation().is_none() && entry.requirement.is_none()),
            "a pinned entry states a version and no requirement"
        );
        let folders: Vec<(&str, &InstallLocation)> = answer
            .install_folders
            .iter()
            .filter_map(|folder| match &folder.origin {
                rift_protocol::read::SourceLocation::Dependency { package } => {
                    Some((package.name.as_str(), &folder.location))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            folders,
            [
                (
                    "serde",
                    &InstallLocation::CargoRegistry("serde-1.0.228".to_owned())
                ),
                (
                    "internal-tool",
                    &InstallLocation::CargoRegistry("internal-tool-2.0.0".to_owned())
                ),
            ],
            "a registry package records its unpacked folder; a git checkout records none"
        );
        assert_eq!(
            answer.inputs,
            [project("Cargo.toml"), project("Cargo.lock")]
        );
        assert!(answer.degradations.is_empty());
    }

    #[test]
    fn test_a_manifest_only_workspace_reports_requirements_and_no_versions() {
        let manifest = "\
[package]
name = \"probe\"
version = \"0.1.0\"

[dependencies]
serde = \"1.2.3\"
pinned = \"=1.2.3\"
local = { path = \"../local\", version = \"0.4\" }
sourced = { git = \"https://example.test/tool\", version = \"0.5\" }
private = { registry = \"internal\", version = \"0.6\" }
inherited = { workspace = true }
renamed = { package = \"real-name\", version = \"2.0\" }

[dev-dependencies]
criterion = \"0.7\"

[build-dependencies]
cc = \"1\"

[workspace.dependencies]
shared = \"3.1\"
";
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/Cargo.toml"), manifest);

        let answer = context(&["Cargo.toml"], &mut inspector);

        assert_eq!(
            spelled(&answer),
            [
                "local: requirement 0.4",
                "pinned: version 1.2.3",
                "private: requirement 0.6",
                "real-name: requirement 2.0",
                "serde: requirement 1.2.3",
                "sourced: requirement 0.5",
                "criterion: requirement 0.7",
                "cc: requirement 1",
                "shared: requirement 3.1"
            ],
            "a bare `1.2.3` is a caret requirement and `=1.2.3` pins one version; a \
             declaration stating no version is not reported"
        );
        let unserved: Vec<(&str, PackageAvailability)> = answer
            .entries
            .iter()
            .filter(|entry| entry.availability != PackageAvailability::Canonical)
            .map(|entry| (entry.name.as_str(), entry.availability))
            .collect();
        assert_eq!(
            unserved,
            [
                ("local", PackageAvailability::Path),
                ("private", PackageAvailability::PrivateRegistry),
                ("sourced", PackageAvailability::Git)
            ]
        );
        assert!(
            answer.install_folders.is_empty(),
            "a manifest alone pins no version, so nothing is located"
        );
        assert!(
            answer
                .entries
                .iter()
                .all(|entry| entry.violation().is_none()),
            "no entry states both selectors or neither"
        );
        assert!(answer.degradations.is_empty());
    }

    #[test]
    fn test_each_lockfile_source_sorts_into_its_kind() {
        let cases = [
            (None, None, None),
            (None, Some(Declaration::Inside), None),
            (None, Some(Declaration::Inherited), None),
            (
                None,
                Some(Declaration::Elsewhere),
                Some(PackageAvailability::Path),
            ),
            (
                Some("registry+https://github.com/rust-lang/crates.io-index"),
                None,
                Some(PackageAvailability::Canonical),
            ),
            (
                Some("sparse+https://index.crates.io/"),
                None,
                Some(PackageAvailability::Canonical),
            ),
            (
                Some("sparse+https://registry.example.test/index/"),
                None,
                Some(PackageAvailability::PrivateRegistry),
            ),
            (
                Some("git+https://github.com/astral-sh/ruff?rev=2b0d21#2b0d210"),
                None,
                Some(PackageAvailability::Git),
            ),
        ];
        for (source, declaration, expected) in cases {
            assert_eq!(
                source_availability(source, declaration),
                expected,
                "{source:?} {declaration:?}"
            );
        }
    }

    #[test]
    fn test_an_unreadable_input_is_reported_as_a_degradation_naming_the_path() {
        let oversized = vec![b'#'; usize::try_from(LOCKFILE_BYTES_MAX).expect("bound fits") + 1];
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/Cargo.lock"), oversized)
            .with_file(format!("{ROOT}/Cargo.toml"), "[dependencies]\nserde = ");

        let answer = context(&["Cargo.toml"], &mut inspector);

        assert!(answer.entries.is_empty());
        assert!(
            answer.degradations[0].starts_with("Cargo.toml: Cargo.toml could not be parsed: "),
            "{:?}",
            answer.degradations[0]
        );
        assert_eq!(
            answer.degradations[1],
            format!(
                "Cargo.toml: Cargo.lock holds {} bytes, past the {LOCKFILE_BYTES_MAX} byte \
                 bound; no packages reported",
                LOCKFILE_BYTES_MAX + 1
            )
        );
    }

    #[test]
    fn test_an_absent_lockfile_and_manifest_report_nothing() {
        let mut inspector = RecordedInspector::default().with_directory(ROOT);

        let answer = context(&["Cargo.toml"], &mut inspector);

        assert!(answer.entries.is_empty());
        assert!(
            answer.degradations.is_empty(),
            "an absent file is the manifest-only case, not a degradation"
        );
    }

    #[test]
    fn test_only_an_exact_operator_over_a_whole_version_pins_one_release() {
        let pinned = ["=1.2.3", " = 1.2.3 ", "=1.2.3-alpha.1", "=1.2.3+build.5"];
        for requirement in pinned {
            assert!(
                matches!(selector(requirement), PackageSelector::Version(_)),
                "{requirement} pins one version"
            );
        }
        let ranged = [
            "1.2.3",
            "^1.2.3",
            "~1.2.3",
            "=1.2",
            "=1",
            "=1.2.*",
            ">=1.2.3, <2.0.0",
            "*",
            "=1.2.3.4",
        ];
        for requirement in ranged {
            assert_eq!(
                selector(requirement),
                PackageSelector::Requirement(requirement.to_owned()),
                "{requirement} admits a range"
            );
        }
    }

    #[test]
    fn test_a_path_dependency_inside_the_root_is_project_source() {
        let manifest = "\
[workspace.dependencies]
helper = { path = \"crates/helper\" }

[dependencies]
helper = { workspace = true }
outside = { path = \"../outside\", version = \"0.1\" }
climber = { path = \"crates/../../climber\" }
linked = { path = \"vendor/linked\" }
";
        let member = "[dependencies]\nhelper = { workspace = true }\n";
        let lockfile = "\
version = 4

[[package]]
name = \"helper\"
version = \"0.1.0\"

[[package]]
name = \"outside\"
version = \"0.1.0\"

[[package]]
name = \"climber\"
version = \"0.2.0\"

[[package]]
name = \"linked\"
version = \"0.3.0\"
";
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/Cargo.toml"), manifest)
            .with_file(format!("{ROOT}/crates/app/Cargo.toml"), member)
            .with_file(format!("{ROOT}/Cargo.lock"), lockfile)
            .with_canonical(ROOT, ROOT)
            .with_canonical(
                format!("{ROOT}/crates/helper"),
                format!("{ROOT}/crates/helper"),
            )
            .with_canonical(format!("{ROOT}/vendor/linked"), "/elsewhere/linked");

        let answer = context(&["Cargo.toml", "crates/app/Cargo.toml"], &mut inspector);

        let reported: Vec<(String, PackageAvailability)> = answer
            .entries
            .iter()
            .map(|entry| (entry.name.clone(), entry.availability))
            .collect();
        assert_eq!(
            reported,
            [
                ("outside".to_owned(), PackageAvailability::Path),
                ("climber".to_owned(), PackageAvailability::Path),
                ("linked".to_owned(), PackageAvailability::Path),
                ("outside".to_owned(), PackageAvailability::Path),
            ],
            "helper is project source; `..` and a link leaving the root are not"
        );
    }

    /// One package several manifests declare: a declaration naming a source anywhere
    /// else decides, then a path inside the root, and inheritance alone says nothing.
    #[test]
    fn test_declarations_join_elsewhere_over_inside_over_inherited() {
        use Declaration::{Elsewhere, Inherited, Inside};

        let cases = [
            (Inside, Elsewhere, Elsewhere),
            (Elsewhere, Inherited, Elsewhere),
            (Inherited, Inside, Inside),
            (Inside, Inside, Inside),
            (Inherited, Inherited, Inherited),
        ];
        for (standing, found, joined) in cases {
            assert_eq!(standing.joined(found), joined, "{standing:?} and {found:?}");
        }
    }
}
