//! The Bun static context: the packages `bun.lock` pins.

use std::path::Path;

use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry, PackageSelector};
use rift_protocol::read::ProjectPath;

use super::{BUN_LOCK_FILE_NAME, LockedPackage, Pin, install_path, parse_lockfile};
use crate::context::ContextAnswer;
use crate::manifest::{WorkspacePaths, file_beside, manifest_directory_path, read_static_file};
use crate::node::{NPM_MANAGER, installed_folder, npm_selector, version_availability};
use crate::resolver::{ContextRequest, StaticInputs};

/// The default registry a Bun tuple may name, trailing separator dropped. Bun leaves the
/// registry element empty for it; every other registry is a private one.
const NPM_REGISTRY_URL: &str = "https://registry.npmjs.org";

/// Reports the packages every `bun.lock` beside a listed manifest pins, each exact one
/// with the install folder its key spells.
///
/// The npm resolver claims the same `package.json` and owns its declarations, so this
/// pass reads lockfiles alone; a requirement for a package pinned here is dropped where
/// the answers merge. A lockfile that stands beside a manifest is an input. An absent
/// lockfile states nothing; one over its bound or unparsable is a degradation naming the
/// path.
pub(super) fn bun_context(
    request: &ContextRequest<'_>,
    inputs: &mut dyn StaticInputs,
) -> ContextAnswer {
    let mut answer = ContextAnswer::default();
    let workspace = WorkspacePaths::new(request.root, inputs);
    for manifest in request.manifests {
        pin_lockfile(request.root, manifest, &workspace, inputs, &mut answer);
    }
    answer
}

/// Reports every package the `bun.lock` beside one manifest pins.
fn pin_lockfile(
    root: &Path,
    manifest: &ProjectPath,
    workspace: &WorkspacePaths,
    inputs: &mut dyn StaticInputs,
    answer: &mut ContextAnswer,
) {
    let directory = manifest_directory_path(root, manifest);
    let observed = read_static_file(&directory, BUN_LOCK_FILE_NAME, inputs);
    if let Err(failure) = &observed
        && failure.is_absent()
    {
        return;
    }
    answer
        .inputs
        .push(file_beside(manifest, BUN_LOCK_FILE_NAME));
    let lockfile = match observed.and_then(|bytes| parse_lockfile(&bytes)) {
        Ok(lockfile) => lockfile,
        Err(failure) => {
            let manifest_path = &manifest.0;
            answer
                .degradations
                .push(format!("{manifest_path}: {failure}; no packages reported"));
            return;
        }
    };
    for (key, package) in &lockfile.packages {
        let Pin::Package { name, version } = package.pin() else {
            continue;
        };
        let Some(availability) = pin_availability(package, version, &directory, workspace, inputs)
        else {
            continue;
        };
        let selector = npm_selector(version);
        if let (PackageSelector::Version(exact), Some(path)) = (&selector, install_path(key)) {
            answer
                .install_folders
                .push(installed_folder(&directory, &path, (name, exact), inputs));
        }
        answer.entries.push(PackageContextEntry::new(
            NPM_MANAGER,
            name,
            selector,
            availability,
        ));
    }
}

/// Whether a global package index can answer for one pinned tuple: its version text
/// decides, and a registry package fetched from a registry the tuple names other than
/// npm's is a private one. `None` for project source.
fn pin_availability(
    package: &LockedPackage,
    version: &str,
    directory: &Path,
    workspace: &WorkspacePaths,
    inputs: &mut dyn StaticInputs,
) -> Option<PackageAvailability> {
    let availability = version_availability(version, directory, workspace, inputs)?;
    let private = package
        .registry()
        .is_some_and(|registry| registry.trim_end_matches('/') != NPM_REGISTRY_URL);
    if availability == PackageAvailability::Canonical && private {
        Some(PackageAvailability::PrivateRegistry)
    } else {
        Some(availability)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BunResolver;
    use crate::context::InstallLocation;
    use crate::fixture::RecordedInspector;
    use crate::resolver::{DependencyResolver, LOCKFILE_BYTES_MAX};

    const ROOT: &str = "/workspace";

    /// A lockfile pinning one registry package, one package from a private registry, one
    /// nested copy, one repository package, a directory inside the root, and the
    /// workspace's own member.
    const LOCKFILE: &str = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "probe", "devDependencies": { "typescript": "5.9.3" } },
  },
  "packages": {
    "typescript": ["typescript@5.9.3", "", {}, "sha512-jl1"],
    "internal": ["internal@2.0.0", "https://npm.example.test/", {}, "sha512-in"],
    "public": ["public@1.0.0", "https://registry.npmjs.org/", {}, "sha512-pu"],
    "typescript/@types/node": ["@types/node@24.3.1", "", {}, "sha512-tn"],
    "tool": ["tool@git+https://example.test/tool.git#abc1234", {}, "abc1234"],
    "api": ["api@workspace:packages/api"],
    "local": ["local@file:packages/local", {}],
    "broken": ["no-separator"],
  }
}
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
        BunResolver::new().context(&request, inspector)
    }

    #[test]
    fn test_a_lockfile_pins_exact_versions_and_sorts_every_reference_by_kind() {
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/bun.lock"), LOCKFILE);

        let answer = context(&["package.json"], &mut inspector);

        let spelled: Vec<String> = answer
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
                format!("{}: {selector} ({:?})", entry.name, entry.availability)
            })
            .collect();
        assert_eq!(
            spelled,
            [
                "internal: version 2.0.0 (PrivateRegistry)",
                "public: version 1.0.0 (Canonical)",
                "tool: requirement git+https://example.test/tool.git#abc1234 (Git)",
                "typescript: version 5.9.3 (Canonical)",
                "@types/node: version 24.3.1 (Canonical)"
            ],
            "a workspace package, a directory inside the root, and a malformed tuple pin \
             nothing"
        );
        let folders: Vec<(String, &InstallLocation)> = answer
            .install_folders
            .iter()
            .map(|folder| (folder.package.name.clone(), &folder.location))
            .collect();
        let at = |path: &str| InstallLocation::Path(Path::new(ROOT).join(path));
        assert_eq!(
            folders,
            [
                ("internal".to_owned(), &at("node_modules/internal")),
                ("public".to_owned(), &at("node_modules/public")),
                ("typescript".to_owned(), &at("node_modules/typescript")),
                (
                    "@types/node".to_owned(),
                    &at("node_modules/typescript/node_modules/@types/node")
                ),
            ],
            "every exact pin gets the folder its key spells; a repository pins no version"
        );
        assert!(
            answer
                .entries
                .iter()
                .all(|entry| entry.violation().is_none())
        );
        assert_eq!(answer.inputs, [project("bun.lock")]);
        assert!(answer.degradations.is_empty());
        assert_eq!(
            answer.entries[3].availability,
            PackageAvailability::Canonical
        );
    }

    #[test]
    fn test_an_absent_lockfile_reports_nothing_and_an_unreadable_one_degrades() {
        let mut inspector = RecordedInspector::default().with_directory(ROOT);
        let answer = context(&["package.json"], &mut inspector);
        assert!(answer.entries.is_empty());
        assert!(answer.inputs.is_empty());
        assert!(answer.degradations.is_empty());

        let oversized = vec![b'{'; usize::try_from(LOCKFILE_BYTES_MAX).expect("bound fits") + 1];
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/bun.lock"), oversized);

        let answer = context(&["package.json"], &mut inspector);

        assert!(answer.entries.is_empty());
        assert_eq!(
            answer.degradations,
            [format!(
                "package.json: bun.lock holds {} bytes, past the {LOCKFILE_BYTES_MAX} byte \
                 bound; no packages reported",
                LOCKFILE_BYTES_MAX + 1
            )]
        );
    }
}
