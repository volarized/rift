//! The npm static context: what `package-lock.json` pins and `package.json` declares.

use std::path::Path;

use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry, PackageSelector};
use rift_protocol::read::ProjectPath;

use super::{PACKAGE_LOCK_FILE_NAME, installed_package, parse_lockfile};
use crate::context::ContextAnswer;
use crate::manifest::{StaticFileFailure, file_beside, manifest_directory_path, read_static_file};
use crate::node::{NPM_MANAGER, PACKAGE_MANIFEST_FILE_NAME, parse_package_manifest};
use crate::resolver::{ContextRequest, StaticInputs};

/// The registry URL prefix a package fetched from the public npm registry resolves from.
const NPM_REGISTRY_URL: &str = "https://registry.npmjs.org/";

/// Reports what each listed manifest's `package-lock.json` pins and what the
/// `package.json` itself declares.
///
/// This resolver owns the `package.json` read for both npm and Bun, which claim the same
/// manifest: a requirement for a package either lockfile pins is dropped where the
/// answers merge, so one selector reaches the context per package. Each manifest and
/// each lockfile beside one is an input. An absent file is the manifest-only case, not a
/// degradation; a file over its bound or unparsable is one, naming the path.
pub(super) fn npm_context(
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

/// Reports every package the `package-lock.json` beside one manifest pins.
fn pin_lockfile(
    root: &Path,
    manifest: &ProjectPath,
    inputs: &mut dyn StaticInputs,
    answer: &mut ContextAnswer,
) {
    let directory = manifest_directory_path(root, manifest);
    let observed = read_static_file(&directory, PACKAGE_LOCK_FILE_NAME, inputs);
    if let Err(failure) = &observed
        && failure.is_absent()
    {
        return;
    }
    answer
        .inputs
        .push(file_beside(manifest, PACKAGE_LOCK_FILE_NAME));
    let lockfile = match observed.and_then(|bytes| parse_lockfile(&bytes)) {
        Ok(lockfile) => lockfile,
        Err(failure) => return report(answer, manifest, &failure),
    };
    let pinned = lockfile.packages.iter().filter_map(|(key, package)| {
        let installed = installed_package(key, package)?;
        Some(PackageContextEntry::new(
            NPM_MANAGER,
            installed.name,
            PackageSelector::Version(installed.version.to_owned()),
            resolved_availability(package.resolved.as_deref()),
        ))
    });
    answer.entries.extend(pinned);
}

/// Reports every requirement one `package.json` declares.
fn declare_manifest(
    root: &Path,
    manifest: &ProjectPath,
    inputs: &mut dyn StaticInputs,
    answer: &mut ContextAnswer,
) {
    let directory = manifest_directory_path(root, manifest);
    let observed = read_static_file(&directory, PACKAGE_MANIFEST_FILE_NAME, inputs);
    let parsed = match observed.and_then(|bytes| parse_package_manifest(&bytes)) {
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

/// Whether a public registry serves the package one lockfile entry resolved from.
///
/// An entry with no `resolved` came from the default registry; a `file:` or repository
/// locator, and any other host, names bytes this machine alone resolves.
fn resolved_availability(resolved: Option<&str>) -> PackageAvailability {
    match resolved {
        None => PackageAvailability::Canonical,
        Some(locator) if locator.starts_with(NPM_REGISTRY_URL) => PackageAvailability::Canonical,
        Some(_) => PackageAvailability::LocalOnly,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NpmResolver;
    use crate::fixture::RecordedInspector;
    use crate::resolver::{DependencyResolver, LOCKFILE_BYTES_MAX};

    const ROOT: &str = "/workspace";

    /// A lockfile pinning one registry package, one tarball, and one linked workspace
    /// package.
    const LOCKFILE: &str = r#"{
  "name": "probe",
  "lockfileVersion": 3,
  "packages": {
    "": { "name": "probe", "dependencies": { "left-pad": "^1.3.0" } },
    "node_modules/left-pad": {
      "version": "1.3.0",
      "resolved": "https://registry.npmjs.org/left-pad/-/left-pad-1.3.0.tgz"
    },
    "node_modules/internal-tool": {
      "version": "2.0.0",
      "resolved": "https://npm.example.test/internal-tool/-/internal-tool-2.0.0.tgz"
    },
    "node_modules/bundled": { "version": "0.1.0" },
    "packages/api": { "name": "api", "version": "0.0.1" },
    "node_modules/api": { "resolved": "packages/api", "link": true }
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
        NpmResolver::new().context(&request, inspector)
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
    fn test_a_lockfile_pins_exact_versions_and_marks_local_only_locators() {
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/package-lock.json"), LOCKFILE);

        let answer = context(&["package.json"], &mut inspector);

        assert_eq!(
            spelled(&answer),
            [
                "bundled: version 0.1.0",
                "internal-tool: version 2.0.0",
                "left-pad: version 1.3.0"
            ],
            "a linked workspace package pins no version of its own"
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
                PackageAvailability::LocalOnly,
                PackageAvailability::Canonical
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
            [project("package.json"), project("package-lock.json")]
        );
        assert!(answer.degradations.is_empty());
    }

    #[test]
    fn test_a_manifest_only_workspace_reports_requirements_and_no_versions() {
        let manifest = r#"{
  "name": "probe",
  "dependencies": {
    "left-pad": "^1.3.0",
    "pinned": "1.3.0",
    "local": "file:../local",
    "sourced": "git+https://example.test/tool.git",
    "shorthand": "user/repo#main",
    "cataloged": "catalog:default",
    "aliased": "npm:react@^17.0.0"
  },
  "devDependencies": { "typescript": "5.9.3" },
  "optionalDependencies": { "fsevents": "~2.3.0" }
}
"#;
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/package.json"), manifest);

        let answer = context(&["package.json"], &mut inspector);

        assert_eq!(
            spelled(&answer),
            [
                "react: requirement ^17.0.0",
                "cataloged: requirement catalog:default",
                "left-pad: requirement ^1.3.0",
                "local: requirement file:../local",
                "pinned: version 1.3.0",
                "shorthand: requirement user/repo#main",
                "sourced: requirement git+https://example.test/tool.git",
                "typescript: version 5.9.3",
                "fsevents: requirement ~2.3.0"
            ],
            "an `npm:` value names the package it aliases"
        );
        let local_only: Vec<&str> = answer
            .entries
            .iter()
            .filter(|entry| entry.availability == PackageAvailability::LocalOnly)
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            local_only,
            ["cataloged", "local", "shorthand", "sourced"],
            "a catalog entry and a repository shorthand name bytes no registry serves"
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
        let oversized = vec![b'{'; usize::try_from(LOCKFILE_BYTES_MAX).expect("bound fits") + 1];
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/package-lock.json"), oversized)
            .with_file(format!("{ROOT}/package.json"), "{\"dependencies\":");

        let answer = context(&["package.json"], &mut inspector);

        assert!(answer.entries.is_empty());
        assert_eq!(
            answer.degradations[0],
            format!(
                "package.json: package-lock.json holds {} bytes, past the \
                 {LOCKFILE_BYTES_MAX} byte bound; no packages reported",
                LOCKFILE_BYTES_MAX + 1
            )
        );
        assert!(
            answer.degradations[1].starts_with("package.json: package.json could not be parsed: "),
            "{:?}",
            answer.degradations[1]
        );
    }

    #[test]
    fn test_an_absent_lockfile_and_manifest_report_nothing() {
        let mut inspector = RecordedInspector::default().with_directory(ROOT);

        let answer = context(&["package.json"], &mut inspector);

        assert!(answer.entries.is_empty());
        assert_eq!(answer.inputs, [project("package.json")]);
        assert!(answer.degradations.is_empty());
    }
}
