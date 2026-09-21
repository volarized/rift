//! The uv static context: what `uv.lock` pins and `pyproject.toml` declares.

use std::collections::BTreeMap;
use std::path::Path;

use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry, PackageSelector};
use rift_protocol::read::ProjectPath;
use serde::Deserialize;

use super::{
    PYPI_MANAGER, UV_LOCK_FILE_NAME, UV_MANIFEST_FILE_NAME, normalized_name, parse_lockfile,
};
use crate::context::{ContextAnswer, is_whole_version};
use crate::manifest::{StaticFileFailure, file_beside, manifest_directory_path, read_static_file};
use crate::resolver::{ContextRequest, StaticInputs};

/// The `source` registry of a distribution the Python Package Index serves, trailing
/// separator dropped. Every other index is one this machine alone resolves.
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
/// The characters a PEP 503 distribution name is spelled with.
const NAME_CHARACTERS: [char; 3] = ['-', '_', '.'];

/// Reports what each listed manifest's `uv.lock` pins and what its `pyproject.toml`
/// declares.
///
/// Each manifest and each lockfile beside one is an input. A requirement for a
/// distribution some lockfile pins is dropped where the answers merge, so one selector
/// reaches the context per distribution. An absent file is the manifest-only case, not a
/// degradation; a file over its bound or unparsable is one, naming the path.
pub(super) fn uv_context(
    request: &ContextRequest<'_>,
    inputs: &mut dyn StaticInputs,
) -> ContextAnswer {
    let mut answer = ContextAnswer::default();
    for manifest in request.manifests {
        answer.inputs.push(manifest.clone());
        pin_lockfile(request.root, manifest, inputs, &mut answer);
        declare_manifest(request.root, manifest, inputs, &mut answer);
    }
    answer
}

/// Reports every distribution the `uv.lock` beside one manifest pins.
fn pin_lockfile(
    root: &Path,
    manifest: &ProjectPath,
    inputs: &mut dyn StaticInputs,
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
    let pinned = lockfile
        .package
        .iter()
        .filter(|package| !package.source.is_member())
        .filter_map(|package| {
            Some(PackageContextEntry::new(
                PYPI_MANAGER,
                &normalized_name(&package.name),
                PackageSelector::Version(package.version.clone()?),
                registry_availability(package.source.registry.as_deref()),
            ))
        });
    answer.entries.extend(pinned);
}

/// Reports every requirement one `pyproject.toml` declares.
fn declare_manifest(
    root: &Path,
    manifest: &ProjectPath,
    inputs: &mut dyn StaticInputs,
    answer: &mut ContextAnswer,
) {
    let directory = manifest_directory_path(root, manifest);
    let observed = read_static_file(&directory, UV_MANIFEST_FILE_NAME, inputs);
    let parsed = match observed.and_then(|bytes| parse_manifest(&bytes)) {
        Ok(parsed) => parsed,
        Err(failure) => return report(answer, manifest, &failure),
    };
    answer.entries.extend(parsed.declared());
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

/// Whether a global package index can answer for a distribution fetched from `registry`.
fn registry_availability(registry: Option<&str>) -> PackageAvailability {
    match registry {
        Some(registry) if registry.trim_end_matches('/') == PYPI_REGISTRY_URL => {
            PackageAvailability::Canonical
        }
        _ => PackageAvailability::LocalOnly,
    }
}

/// The `pyproject.toml` document, the dependency lists this pass reads.
#[derive(Deserialize)]
struct Manifest {
    project: Option<Project>,
    #[serde(default, rename = "dependency-groups")]
    dependency_groups: BTreeMap<String, Vec<GroupEntry>>,
}

/// The `[project]` table, the one key this pass reads.
#[derive(Deserialize)]
struct Project {
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
    /// One entry per declared requirement stating a version, in project then group order.
    fn declared(&self) -> impl Iterator<Item = PackageContextEntry> {
        let project = self
            .project
            .iter()
            .flat_map(|project| project.dependencies.iter().map(String::as_str));
        let groups = self
            .dependency_groups
            .values()
            .flatten()
            .filter_map(GroupEntry::requirement);
        project.chain(groups).filter_map(declared_entry)
    }
}

/// Parses `pyproject.toml` bytes, naming the parser's message when they are not its
/// document.
fn parse_manifest(bytes: &[u8]) -> Result<Manifest, StaticFileFailure> {
    toml::from_slice(bytes).map_err(|error| {
        StaticFileFailure::unparsable(UV_MANIFEST_FILE_NAME, error.message().to_owned())
    })
}

/// The entry one PEP 508 requirement declares.
///
/// A requirement stating no specifier names no version, so it is not reported: the
/// context carries one selector per entry and has none to state for it.
fn declared_entry(requirement: &str) -> Option<PackageContextEntry> {
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
    Some(PackageContextEntry::new(
        PYPI_MANAGER,
        &normalized_name(name),
        selector(specifier),
        specifier_availability(specifier),
    ))
}

/// Whether a global package index can answer for a declared specifier: a direct
/// reference names a URL this machine alone resolves.
fn specifier_availability(specifier: &str) -> PackageAvailability {
    if specifier.starts_with(DIRECT_REFERENCE) {
        PackageAvailability::LocalOnly
    } else {
        PackageAvailability::Canonical
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
    /// private index, and the workspace's own project.
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
    fn test_a_lockfile_pins_exact_versions_and_marks_a_private_index_local_only() {
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/uv.lock"), LOCKFILE);

        let answer = context(&["pyproject.toml"], &mut inspector);

        assert_eq!(
            spelled(&answer),
            [
                "typing-extensions: version 4.15.0",
                "internal-tool: version 2.0.0"
            ],
            "the workspace's own project pins nothing, and a name normalizes"
        );
        assert_eq!(
            answer
                .entries
                .iter()
                .map(|entry| entry.availability)
                .collect::<Vec<_>>(),
            [
                PackageAvailability::Canonical,
                PackageAvailability::LocalOnly
            ]
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
                "pinned: requirement === 1.2.3",
                "pytest: version 8.4.2",
                "ruff: requirement >=0.14"
            ],
            "a requirement stating no specifier, and one stating no name, are not reported"
        );
        let local_only: Vec<&str> = answer
            .entries
            .iter()
            .filter(|entry| entry.availability == PackageAvailability::LocalOnly)
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(local_only, ["tool"]);
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
}
