//! The uv static context: what `uv.lock` pins and `pyproject.toml` declares.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry, PackageSelector};
use rift_protocol::read::{PackageIdentity, ProjectPath};
use serde::Deserialize;

use super::environment::SitePackages;
use super::{
    LockedSource, PYPI_MANAGER, UV_LOCK_FILE_NAME, UV_MANIFEST_FILE_NAME, normalized_name,
    parse_lockfile,
};
use crate::context::{ContextAnswer, InstallFolder, InstallLocation, is_whole_version};
use crate::manifest::{
    StaticFileFailure, WorkspacePaths, file_beside, manifest_directory_path, read_static_file,
};
use crate::resolver::{ContextRequest, StaticInputs};

/// The `source` registry of a distribution the Python Package Index serves, trailing
/// separator dropped. Every other index is a private registry.
const PYPI_REGISTRY_URL: &str = "https://pypi.org/simple";
/// The operator that pins one exact version in a PEP 508 requirement.
const EXACT_OPERATOR: &str = "==";
/// The operator that pins one version by arbitrary equality, which matches by string
/// rather than by release, so it names no comparable version.
const ARBITRARY_OPERATOR: &str = "===";
/// The separator between the clauses of a multi-clause PEP 508 specifier.
const CLAUSE_SEPARATOR: char = ',';
/// The character opening a PEP 508 environment marker.
const MARKER_SEPARATOR: char = ';';
/// The character opening a PEP 508 extras list.
const EXTRAS_OPEN: char = '[';
/// The character closing a PEP 508 extras list.
const EXTRAS_CLOSE: char = ']';
/// The character opening a PEP 508 direct reference: a URL in place of a specifier.
const DIRECT_REFERENCE: char = '@';
/// The prefixes of a direct reference to a URL no index serves.
const URL_PREFIXES: [&str; 2] = ["http://", "https://"];
/// The prefix of a direct reference to a git repository.
const GIT_REFERENCE_PREFIX: &str = "git+";
/// The file name endings of a built or source distribution. The local index reads no
/// archive, so one inside the root is not project source.
const ARCHIVE_SUFFIXES: [&str; 3] = [".whl", ".tar.gz", ".zip"];
/// The characters a PEP 503 distribution name is spelled with.
const NAME_CHARACTERS: [char; 3] = ['-', '_', '.'];

/// Reports what each listed manifest's `uv.lock` pins and what its `pyproject.toml`
/// declares. Each pinned distribution the environment beside the lockfile installed
/// records the import folders its `RECORD` lists.
///
/// Each manifest and each lockfile beside one is an input. A requirement for a
/// distribution some lockfile pins is dropped where the answers merge, so one selector
/// reaches the context per distribution. An absent file is the manifest-only case, not a
/// degradation; a file over its bound or unparsable is one, naming the path.
///
/// The lockfiles and manifests are read first: a distribution any lockfile locks from a
/// directory is declared by path, so a member's `inner>=0.3` names the workspace's own
/// project, not the index's; and a `{ workspace = true }` source is project source only
/// when a claimed manifest inside the root declares that name.
pub(super) fn uv_context(
    request: &ContextRequest<'_>,
    inputs: &mut dyn StaticInputs,
) -> ContextAnswer {
    let mut answer = ContextAnswer::default();
    let workspace = WorkspacePaths::new(request.root, inputs);
    let mut located = BTreeMap::new();
    let mut parsed = Vec::new();
    for manifest in request.manifests {
        answer.inputs.push(manifest.clone());
        pin_lockfile(
            request.root,
            manifest,
            &workspace,
            inputs,
            &mut located,
            &mut answer,
        );
        let directory = manifest_directory_path(request.root, manifest);
        let observed = read_static_file(&directory, UV_MANIFEST_FILE_NAME, inputs);
        match observed.and_then(|bytes| parse_manifest(&bytes)) {
            Ok(document) => parsed.push((directory, document)),
            Err(failure) => report(&mut answer, manifest, &failure),
        }
    }
    let claimed_names: BTreeSet<String> = parsed
        .iter()
        .filter_map(|(_, document)| document.name())
        .map(normalized_name)
        .collect();
    for (directory, document) in &parsed {
        declare_manifest(
            directory,
            document,
            &workspace,
            &located,
            &claimed_names,
            inputs,
            &mut answer,
        );
    }
    answer
}

/// Reports every distribution the `uv.lock` beside one manifest pins, and records each
/// one it locks from a directory by whether the directory lies inside the root.
fn pin_lockfile(
    root: &Path,
    manifest: &ProjectPath,
    workspace: &WorkspacePaths,
    inputs: &mut dyn StaticInputs,
    located: &mut BTreeMap<String, bool>,
    answer: &mut ContextAnswer,
) {
    let directory = manifest_directory_path(root, manifest);
    let observed = read_static_file(&directory, UV_LOCK_FILE_NAME, inputs);
    if let Err(failure) = &observed
        && failure.is_absent()
    {
        return;
    }
    answer.inputs.push(file_beside(manifest, UV_LOCK_FILE_NAME));
    let lockfile = match observed.and_then(|bytes| parse_lockfile(&bytes)) {
        Ok(lockfile) => lockfile,
        Err(failure) => return report(answer, manifest, &failure),
    };
    let site_packages = SitePackages::observe(&directory, inputs);
    for package in &lockfile.package {
        let name = normalized_name(&package.name);
        if let Some(path) = package.source.directory_path() {
            let inside = workspace.contains(&directory.join(path), inputs);
            located
                .entry(name.clone())
                .and_modify(|standing| *standing &= inside)
                .or_insert(inside);
            if inside {
                continue;
            }
        }
        let Some(version) = package.version.clone() else {
            continue;
        };
        if let Some(site_packages) = &site_packages {
            answer.install_folders.extend(
                site_packages
                    .import_roots(&name, &version, inputs)
                    .into_iter()
                    .map(|root| InstallFolder {
                        package: PackageIdentity {
                            manager: PYPI_MANAGER.to_owned(),
                            name: name.clone(),
                            version: version.clone(),
                        },
                        location: InstallLocation::ImportRoot {
                            site_packages: site_packages.directory().to_path_buf(),
                            root,
                        },
                    }),
            );
        }
        answer.entries.push(PackageContextEntry::new(
            PYPI_MANAGER,
            &name,
            PackageSelector::Version(version),
            package.source.availability(),
        ));
    }
}

/// Reports every requirement one `pyproject.toml`, standing in `directory`, declares. A
/// distribution a lockfile locks from a directory inside the root is project source and
/// is left out; one outside it is a `path` entry. Otherwise the manifest's
/// `[tool.uv.sources]` entry, then the specifier, decides.
fn declare_manifest(
    directory: &Path,
    document: &Manifest,
    workspace: &WorkspacePaths,
    located: &BTreeMap<String, bool>,
    claimed_names: &BTreeSet<String>,
    inputs: &mut dyn StaticInputs,
    answer: &mut ContextAnswer,
) {
    let sources = document.sources();
    let declared = document
        .requirements()
        .filter_map(declared_requirement)
        .filter_map(|(name, specifier)| {
            let availability = match located.get(&name) {
                Some(true) => return None,
                Some(false) => PackageAvailability::Path,
                None => match sources.get(&name) {
                    Some(source) => {
                        let claimed = claimed_names.contains(&name);
                        source.availability(claimed, directory, workspace, inputs)?
                    }
                    None => specifier_availability(specifier),
                },
            };
            Some(PackageContextEntry::new(
                PYPI_MANAGER,
                &name,
                selector(specifier),
                availability,
            ))
        });
    answer.entries.extend(declared);
}

impl LockedSource {
    /// The directory a package locks from, relative to the lockfile: a member, the
    /// root project, or a path dependency on a directory, editable or not.
    fn directory_path(&self) -> Option<&str> {
        self.editable
            .as_deref()
            .or(self.virtual_directory.as_deref())
            .or(self.directory.as_deref())
    }

    /// Whether a global package index can answer for a distribution locked from this
    /// source: the Python Package Index can. A direct URL is a `url` entry, a repository
    /// a `git` one, every other index a private registry, and an archive or a directory
    /// outside the root a `path` entry.
    fn availability(&self) -> PackageAvailability {
        if self.url.is_some() {
            return PackageAvailability::Url;
        }
        if self.git.is_some() {
            return PackageAvailability::Git;
        }
        match self.registry.as_deref() {
            Some(registry) if registry.trim_end_matches('/') == PYPI_REGISTRY_URL => {
                PackageAvailability::Canonical
            }
            Some(_) => PackageAvailability::PrivateRegistry,
            None => PackageAvailability::Path,
        }
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

/// The `pyproject.toml` document, the dependency lists this pass reads and the sources
/// uv resolves them from.
#[derive(Deserialize)]
struct Manifest {
    project: Option<Project>,
    #[serde(default, rename = "dependency-groups")]
    dependency_groups: BTreeMap<String, Vec<GroupEntry>>,
    tool: Option<Tool>,
}

/// The `[tool]` table, the one key this pass reads.
#[derive(Deserialize)]
struct Tool {
    uv: Option<UvTool>,
}

/// The `[tool.uv]` table, the one key this pass reads.
#[derive(Deserialize)]
struct UvTool {
    #[serde(default)]
    sources: BTreeMap<String, Sources>,
}

/// One `[tool.uv.sources]` value: one source, or several split by marker.
#[derive(Deserialize)]
#[serde(untagged)]
enum Sources {
    One(Source),
    Several(Vec<Source>),
}

/// One uv source table, the keys naming where the distribution comes from.
#[derive(Deserialize)]
struct Source {
    workspace: Option<bool>,
    path: Option<String>,
    git: Option<String>,
    url: Option<String>,
    index: Option<String>,
}

impl Sources {
    /// Whether a global package index can answer for a distribution from these
    /// sources, `None` for project source; `claimed` says whether a claimed manifest
    /// declares the distribution's name. Of several, the first that is not project
    /// source decides.
    fn availability(
        &self,
        claimed: bool,
        directory: &Path,
        workspace: &WorkspacePaths,
        inputs: &mut dyn StaticInputs,
    ) -> Option<PackageAvailability> {
        match self {
            Self::One(source) => source.availability(claimed, directory, workspace, inputs),
            Self::Several(sources) => sources
                .iter()
                .find_map(|source| source.availability(claimed, directory, workspace, inputs)),
        }
    }
}

impl Source {
    /// Whether a global package index can answer for a distribution from this source,
    /// `None` for project source: a workspace member a claimed manifest declares
    /// (`claimed`), or a directory inside the root. A member no claimed manifest declares
    /// stands outside the root, where the local index does not read it.
    fn availability(
        &self,
        claimed: bool,
        directory: &Path,
        workspace: &WorkspacePaths,
        inputs: &mut dyn StaticInputs,
    ) -> Option<PackageAvailability> {
        if self.workspace == Some(true) {
            return (!claimed).then_some(PackageAvailability::Path);
        }
        if let Some(path) = self.path.as_deref() {
            let archive = ARCHIVE_SUFFIXES.iter().any(|suffix| path.ends_with(suffix));
            return (archive || !workspace.contains(&directory.join(path), inputs))
                .then_some(PackageAvailability::Path);
        }
        if self.url.is_some() {
            return Some(PackageAvailability::Url);
        }
        if self.git.is_some() {
            return Some(PackageAvailability::Git);
        }
        if self.index.is_some() {
            return Some(PackageAvailability::PrivateRegistry);
        }
        Some(PackageAvailability::Canonical)
    }
}

/// The `[project]` table, the two keys this pass reads.
#[derive(Deserialize)]
struct Project {
    name: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

/// One `[dependency-groups]` element: a PEP 508 requirement, or a table including
/// another group, whose own elements this pass already reads under that group's key.
#[derive(Deserialize)]
#[serde(untagged)]
enum GroupEntry {
    Requirement(String),
    IncludedGroup(
        #[expect(
            dead_code,
            reason = "the table is matched only to skip it: the group it names is read \
                      under that group's own key"
        )]
        BTreeMap<String, String>,
    ),
}

impl GroupEntry {
    /// The requirement this element states, absent for an included group.
    fn requirement(&self) -> Option<&str> {
        match self {
            Self::Requirement(requirement) => Some(requirement.as_str()),
            Self::IncludedGroup(_) => None,
        }
    }
}

impl Manifest {
    /// The `[project]` name this manifest declares.
    fn name(&self) -> Option<&str> {
        self.project.as_ref()?.name.as_deref()
    }

    /// Every declared PEP 508 requirement, in project then group order.
    fn requirements(&self) -> impl Iterator<Item = &str> {
        let project = self
            .project
            .iter()
            .flat_map(|project| project.dependencies.iter().map(String::as_str));
        let groups = self
            .dependency_groups
            .values()
            .flatten()
            .filter_map(GroupEntry::requirement);
        project.chain(groups)
    }

    /// The `[tool.uv.sources]` entries, keyed by normalized distribution name.
    fn sources(&self) -> BTreeMap<String, &Sources> {
        self.tool
            .iter()
            .filter_map(|tool| tool.uv.as_ref())
            .flat_map(|uv| uv.sources.iter())
            .map(|(name, sources)| (normalized_name(name), sources))
            .collect()
    }
}

/// Parses `pyproject.toml` bytes, naming the parser's message when they are not its
/// document.
fn parse_manifest(bytes: &[u8]) -> Result<Manifest, StaticFileFailure> {
    toml::from_slice(bytes).map_err(|error| {
        StaticFileFailure::unparsable(UV_MANIFEST_FILE_NAME, error.message().to_owned())
    })
}

/// The normalized name and specifier one PEP 508 requirement declares.
///
/// A requirement stating no specifier names no version, so it is not reported: the
/// context carries one selector per entry and has none to state for it.
fn declared_requirement(requirement: &str) -> Option<(String, &str)> {
    let stated = requirement
        .split_once(MARKER_SEPARATOR)
        .map_or(requirement, |(dependency, _)| dependency)
        .trim();
    let name_bytes = stated
        .find(|character: char| {
            !character.is_ascii_alphanumeric() && !NAME_CHARACTERS.contains(&character)
        })
        .unwrap_or(stated.len());
    let (name, rest) = stated.split_at(name_bytes);
    if name.is_empty() {
        return None;
    }
    let specifier = rest.trim_start().strip_prefix(EXTRAS_OPEN).map_or_else(
        || rest.trim(),
        |extras| {
            extras
                .split_once(EXTRAS_CLOSE)
                .map_or("", |(_, after)| after.trim())
        },
    );
    if specifier.is_empty() {
        return None;
    }
    Some((normalized_name(name), specifier))
}

/// Whether a global package index can answer for a declared specifier: a direct
/// reference to an `http:` or `https:` URL is a `url` entry, one to a `git+` URL a `git`
/// entry, and any other direct reference, such as `file:`, a `path` one.
fn specifier_availability(specifier: &str) -> PackageAvailability {
    let Some(reference) = specifier.strip_prefix(DIRECT_REFERENCE) else {
        return PackageAvailability::Canonical;
    };
    let reference = reference.trim_start();
    if URL_PREFIXES
        .iter()
        .any(|prefix| reference.starts_with(prefix))
    {
        PackageAvailability::Url
    } else if reference.starts_with(GIT_REFERENCE_PREFIX) {
        PackageAvailability::Git
    } else {
        PackageAvailability::Path
    }
}

/// The selector one PEP 508 specifier states.
fn selector(specifier: &str) -> PackageSelector {
    match pinned_version(specifier) {
        Some(version) => PackageSelector::Version(version.to_owned()),
        None => PackageSelector::Requirement(specifier.to_owned()),
    }
}

/// The one exact version a PEP 508 specifier pins, absent when it admits a range.
///
/// Only `==` over a whole version names one release: a specifier of several clauses
/// names a range, and `===` compares the version string rather than the release.
fn pinned_version(specifier: &str) -> Option<&str> {
    if specifier.contains(CLAUSE_SEPARATOR) || specifier.starts_with(ARBITRARY_OPERATOR) {
        return None;
    }
    let exact = specifier.strip_prefix(EXACT_OPERATOR)?.trim();
    is_whole_version(exact).then_some(exact)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UvResolver;
    use crate::fixture::RecordedInspector;
    use crate::resolver::{DependencyResolver, LOCKFILE_BYTES_MAX};

    const ROOT: &str = "/workspace";

    /// A lockfile pinning one distribution from the Python Package Index, one from a
    /// private index, one from a git repository, one from an archive path, and the
    /// workspace's own project.
    const LOCKFILE: &str = r#"version = 1

[[package]]
name = "probe"
version = "0.1.0"
source = { editable = "." }

[[package]]
name = "Typing-Extensions"
version = "4.15.0"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "internal-tool"
version = "2.0.0"
source = { registry = "https://pypi.example.test/simple" }

[[package]]
name = "sourced"
version = "0.5.0"
source = { git = "https://example.test/sourced.git?rev=v0.5.0#0123456789abcdef" }

[[package]]
name = "vendored"
version = "1.0"
source = { path = "vendor/vendored-1.0-py3-none-any.whl" }
"#;

    fn project(path: &str) -> ProjectPath {
        ProjectPath(path.to_owned())
    }

    fn context(manifests: &[&str], inspector: &mut RecordedInspector) -> ContextAnswer {
        let manifests: Vec<ProjectPath> = manifests.iter().map(|path| project(path)).collect();
        let request = ContextRequest {
            root: Path::new(ROOT),
            manifests: &manifests,
        };
        UvResolver::new().context(&request, inspector)
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
    fn test_a_lockfile_pins_exact_versions_and_sorts_every_source_by_kind() {
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/uv.lock"), LOCKFILE);

        let answer = context(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            reported(&answer),
            [
                (
                    "typing-extensions: version 4.15.0".to_owned(),
                    PackageAvailability::Canonical
                ),
                (
                    "internal-tool: version 2.0.0".to_owned(),
                    PackageAvailability::PrivateRegistry
                ),
                (
                    "sourced: version 0.5.0".to_owned(),
                    PackageAvailability::Git
                ),
                (
                    "vendored: version 1.0".to_owned(),
                    PackageAvailability::Path
                ),
            ],
            "the workspace's own project pins nothing, and a name normalizes"
        );
        assert!(
            answer.install_folders.is_empty(),
            "no environment stands beside the lockfile"
        );
        assert!(
            answer
                .entries
                .iter()
                .all(|entry| entry.violation().is_none() && entry.requirement.is_none())
        );
        assert_eq!(
            answer.inputs,
            [project("pyproject.toml"), project("uv.lock")]
        );
        assert!(answer.degradations.is_empty());
    }

    #[test]
    fn test_a_manifest_only_project_reports_requirements_and_no_versions() {
        let manifest = r#"[project]
name = "probe"
dependencies = [
  "httpx>=0.28",
  "Typing_Extensions==4.15.0",
  "rich[jupyter] >= 13, < 14",
  "tool @ https://example.test/tool-1.0.tar.gz",
  "repo @ git+https://example.test/repo.git@v1",
  "local @ file:///opt/local-1.0.tar.gz",
  "requests",
  "pinned === 1.2.3",
  "== 1.0",
]

[dependency-groups]
dev = ["pytest==8.4.2", { include-group = "lint" }]
lint = ["ruff>=0.14"]
"#;
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/pyproject.toml"), manifest);

        let answer = context(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            spelled(&answer),
            [
                "httpx: requirement >=0.28",
                "typing-extensions: version 4.15.0",
                "rich: requirement >= 13, < 14",
                "tool: requirement @ https://example.test/tool-1.0.tar.gz",
                "repo: requirement @ git+https://example.test/repo.git@v1",
                "local: requirement @ file:///opt/local-1.0.tar.gz",
                "pinned: requirement === 1.2.3",
                "pytest: version 8.4.2",
                "ruff: requirement >=0.14"
            ],
            "a requirement stating no specifier, and one stating no name, are not reported"
        );
        let sourced: Vec<(&str, PackageAvailability)> = answer
            .entries
            .iter()
            .filter(|entry| entry.availability != PackageAvailability::Canonical)
            .map(|entry| (entry.name.as_str(), entry.availability))
            .collect();
        assert_eq!(
            sourced,
            [
                ("tool", PackageAvailability::Url),
                ("repo", PackageAvailability::Git),
                ("local", PackageAvailability::Path)
            ]
        );
        assert!(
            answer
                .entries
                .iter()
                .all(|entry| entry.violation().is_none())
        );
    }

    #[test]
    fn test_an_unreadable_input_is_reported_as_a_degradation_naming_the_path() {
        let oversized = vec![b'#'; usize::try_from(LOCKFILE_BYTES_MAX).expect("bound fits") + 1];
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/uv.lock"), oversized)
            .with_file(format!("{ROOT}/pyproject.toml"), "[project]\nname = ");

        let answer = context(&["pyproject.toml"], &mut inspector);

        assert!(answer.entries.is_empty());
        assert_eq!(
            answer.degradations[0],
            format!(
                "pyproject.toml: uv.lock holds {} bytes, past the {LOCKFILE_BYTES_MAX} byte \
                 bound; no packages reported",
                LOCKFILE_BYTES_MAX + 1
            )
        );
        assert!(
            answer.degradations[1]
                .starts_with("pyproject.toml: pyproject.toml could not be parsed: "),
            "{:?}",
            answer.degradations[1]
        );
    }

    #[test]
    fn test_an_absent_lockfile_and_manifest_report_nothing() {
        let mut inspector = RecordedInspector::default().with_directory(ROOT);

        let answer = context(&["pyproject.toml"], &mut inspector);

        assert!(answer.entries.is_empty());
        assert_eq!(answer.inputs, [project("pyproject.toml")]);
        assert!(answer.degradations.is_empty());
    }

    /// `<name>: <selector>` and availability per entry, in answer order.
    fn reported(answer: &ContextAnswer) -> Vec<(String, PackageAvailability)> {
        spelled(answer)
            .into_iter()
            .zip(answer.entries.iter().map(|entry| entry.availability))
            .collect()
    }

    #[test]
    fn test_a_directory_inside_the_root_is_project_source() {
        // `uv lock --offline` (uv 0.9.5) over a workspace with one member, a directory
        // inside the root, one outside it, and an editable one outside it.
        let manifest = r#"[project]
name = "probe"
version = "0.1.0"
requires-python = ">=3.11"
dependencies = ["member", "inner>=0.3", "outer", "editable-outer"]

[tool.uv.workspace]
members = ["packages/*"]

[tool.uv.sources]
member = { workspace = true }
inner = { path = "libs/inner" }
outer = { path = "../outer" }
editable-outer = { path = "../editable-outer", editable = true }
"#;
        let lockfile = r#"version = 1
revision = 3
requires-python = ">=3.11"

[manifest]
members = [
    "member",
    "probe",
]

[[package]]
name = "editable-outer"
version = "1.1.0"
source = { editable = "../editable-outer" }

[[package]]
name = "inner"
version = "0.3.0"
source = { directory = "libs/inner" }

[[package]]
name = "member"
version = "0.2.0"
source = { editable = "packages/member" }

[[package]]
name = "outer"
version = "1.0.0"
source = { directory = "../outer" }

[[package]]
name = "probe"
version = "0.1.0"
source = { virtual = "." }
"#;
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/pyproject.toml"), manifest)
            .with_file(format!("{ROOT}/uv.lock"), lockfile);

        let answer = context(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            reported(&answer),
            [
                (
                    "editable-outer: version 1.1.0".to_owned(),
                    PackageAvailability::Path
                ),
                ("outer: version 1.0.0".to_owned(), PackageAvailability::Path),
            ],
            "`inner` and `member` lie inside the root, so `inner>=0.3` reaches no index; \
             an editable directory outside it is no member"
        );
    }

    /// uv writes no `version` for a source tree whose version is dynamic, so the lockfile
    /// pins nothing for it, and the manifest's requirement for it, a directory outside the
    /// root, goes out as a `path` entry.
    #[test]
    fn test_a_dynamic_version_directory_outside_the_root_reports_the_requirement() {
        let manifest = r#"[project]
name = "probe"
version = "0.1.0"
dependencies = ["sibling>=2"]

[tool.uv.sources]
sibling = { path = "../sibling", editable = true }
"#;
        let lockfile = r#"version = 1
revision = 3

[[package]]
name = "probe"
version = "0.1.0"
source = { virtual = "." }

[[package]]
name = "sibling"
source = { editable = "../sibling" }
"#;
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/pyproject.toml"), manifest)
            .with_file(format!("{ROOT}/uv.lock"), lockfile);

        let answer = context(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            reported(&answer),
            [(
                "sibling: requirement >=2".to_owned(),
                PackageAvailability::Path
            )]
        );
        assert!(answer.degradations.is_empty());
    }

    #[test]
    fn test_a_nested_project_locking_the_root_reports_it_as_project_source() {
        // ~/projects/crosswire/bench/py311 locks the repository root it sits two levels
        // below; the real lock carries no version, which the fixture adds.
        let manifest = r#"[project]
name = "crosswire-bench-py311"
version = "0.0.0"
dependencies = ["crosswire>=0.1", "typer>=0.15"]

[tool.uv.sources]
crosswire = { path = "../.." }
"#;
        let lockfile = r#"version = 1

[[package]]
name = "crosswire"
version = "0.1.0"
source = { directory = "../../" }

[[package]]
name = "typer"
version = "0.19.2"
source = { registry = "https://pypi.org/simple" }
"#;
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/bench/py311/pyproject.toml"), manifest)
            .with_file(format!("{ROOT}/bench/py311/uv.lock"), lockfile);

        let answer = context(&["bench/py311/pyproject.toml"], &mut inspector);

        assert_eq!(
            reported(&answer),
            [
                (
                    "typer: version 0.19.2".to_owned(),
                    PackageAvailability::Canonical
                ),
                (
                    "typer: requirement >=0.15".to_owned(),
                    PackageAvailability::Canonical
                ),
            ]
        );
    }

    #[test]
    fn test_a_manifest_source_decides_without_a_lockfile() {
        let manifest = r#"[project]
name = "probe"
dependencies = [
  "inner>=0.3",
  "outer>=1",
  "wheel>=1",
  "sourced>=1",
  "fetched>=1",
  "member>=0.2",
  "mirrored>=1",
  "marked>=1",
  "unlocated>=1",
]

[tool.uv.sources]
inner = { path = "libs/inner" }
outer = { path = "../outer" }
wheel = { path = "vendor/wheel-1.0-py3-none-any.whl" }
sourced = { git = "https://example.test/sourced.git" }
fetched = { url = "https://example.test/fetched-1.0-py3-none-any.whl" }
member = { workspace = true }
mirrored = { index = "internal" }
marked = [
  { path = "libs/marked", marker = "sys_platform == 'linux'" },
  { url = "https://example.test/marked-1.0.tar.gz", marker = "sys_platform != 'linux'" },
]
unlocated = { marker = "sys_platform == 'linux'" }
"#;
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/pyproject.toml"), manifest)
            .with_file(
                format!("{ROOT}/packages/member/pyproject.toml"),
                "[project]\nname = \"Member\"\n",
            );

        let answer = context(
            &["packages/member/pyproject.toml", "pyproject.toml"],
            &mut inspector,
        );

        assert_eq!(
            reported(&answer),
            [
                (
                    "outer: requirement >=1".to_owned(),
                    PackageAvailability::Path
                ),
                (
                    "wheel: requirement >=1".to_owned(),
                    PackageAvailability::Path
                ),
                (
                    "sourced: requirement >=1".to_owned(),
                    PackageAvailability::Git
                ),
                (
                    "fetched: requirement >=1".to_owned(),
                    PackageAvailability::Url
                ),
                (
                    "mirrored: requirement >=1".to_owned(),
                    PackageAvailability::PrivateRegistry
                ),
                (
                    "marked: requirement >=1".to_owned(),
                    PackageAvailability::Url
                ),
                (
                    "unlocated: requirement >=1".to_owned(),
                    PackageAvailability::Canonical
                ),
            ],
            "a directory inside the root and a member are project source; an archive \
             inside it is not; a source naming no location leaves the registry deciding"
        );
    }

    #[test]
    fn test_a_direct_url_source_is_a_url_entry() {
        let lockfile = r#"version = 1

[[package]]
name = "fetched"
version = "1.0"
source = { url = "https://example.test/fetched-1.0-py3-none-any.whl" }
"#;
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/uv.lock"), lockfile);

        let answer = context(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            reported(&answer),
            [("fetched: version 1.0".to_owned(), PackageAvailability::Url)]
        );
    }

    #[test]
    fn test_a_workspace_source_is_project_source_only_when_a_claimed_manifest_declares_it() {
        // A uv workspace served at `packages/app`: the workspace root, its `uv.lock`, and
        // the sibling `shared` stand outside the served root; `plugin` stands inside.
        let app = r#"[project]
name = "app"
dependencies = ["shared>=0.1", "plugin>=0.1", "httpx>=0.28"]

[tool.uv.sources]
shared = { workspace = true }
plugin = { workspace = true }
"#;
        let files = [
            (
                "/mono/pyproject.toml",
                "[tool.uv.workspace]\nmembers = [\"packages/*\", \"packages/app/plugins/*\"]\n",
            ),
            ("/mono/packages/app/pyproject.toml", app),
            (
                "/mono/packages/app/plugins/plugin/pyproject.toml",
                "[project]\nname = \"plugin\"\n",
            ),
            (
                "/mono/packages/shared/pyproject.toml",
                "[project]\nname = \"shared\"\n",
            ),
        ];
        let inspector = || {
            files
                .iter()
                .fold(RecordedInspector::default(), |inspector, (path, text)| {
                    inspector.with_file(*path, *text)
                })
        };
        let context_at = |root: &str, manifests: &[&str]| {
            let manifests: Vec<ProjectPath> = manifests.iter().map(|path| project(path)).collect();
            let request = ContextRequest {
                root: Path::new(root),
                manifests: &manifests,
            };
            UvResolver::new().context(&request, &mut inspector())
        };

        let below = context_at(
            "/mono/packages/app",
            &["plugins/plugin/pyproject.toml", "pyproject.toml"],
        );
        let whole = context_at(
            "/mono",
            &[
                "packages/app/plugins/plugin/pyproject.toml",
                "packages/app/pyproject.toml",
                "packages/shared/pyproject.toml",
                "pyproject.toml",
            ],
        );

        assert_eq!(
            reported(&below),
            [
                (
                    "shared: requirement >=0.1".to_owned(),
                    PackageAvailability::Path
                ),
                (
                    "httpx: requirement >=0.28".to_owned(),
                    PackageAvailability::Canonical
                ),
            ],
            "served at `packages/app`, `shared` stands outside the root and `plugin` inside"
        );
        assert_eq!(
            reported(&whole),
            [(
                "httpx: requirement >=0.28".to_owned(),
                PackageAvailability::Canonical
            )]
        );
    }

    #[test]
    fn test_a_pinned_distribution_records_the_import_roots_its_record_lists() {
        let site_packages = format!("{ROOT}/.venv/lib/python3.12/site-packages");
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/uv.lock"), LOCKFILE)
            .with_file(
                format!("{site_packages}/typing_extensions-4.15.0.dist-info/RECORD"),
                "typing_extensions.py,,
typing_extensions-4.15.0.dist-info/RECORD,,
",
            )
            .with_file(
                format!("{site_packages}/internal_tool-2.0.0.dist-info/RECORD"),
                "internal_tool/__init__.py,,
../../../bin/internal-tool,,
",
            );

        let answer = context(&["pyproject.toml"], &mut inspector);

        let folders: Vec<(String, &InstallLocation)> = answer
            .install_folders
            .iter()
            .map(|folder| {
                let package = &folder.package;
                (
                    format!("{}/{}@{}", package.manager, package.name, package.version),
                    &folder.location,
                )
            })
            .collect();
        let at = |root: &str| InstallLocation::ImportRoot {
            site_packages: std::path::PathBuf::from(&site_packages),
            root: root.to_owned(),
        };
        assert_eq!(
            folders,
            [
                (
                    "pypi/typing-extensions@4.15.0".to_owned(),
                    &at("typing_extensions.py")
                ),
                ("pypi/internal-tool@2.0.0".to_owned(), &at("internal_tool")),
            ],
            "a distribution the environment does not hold records no folder"
        );
    }
}
