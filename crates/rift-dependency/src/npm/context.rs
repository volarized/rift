//! The npm static context: what `package-lock.json` pins and `package.json` declares.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry, PackageSelector};
use rift_protocol::read::ProjectPath;

use super::{
    LockedPackage, Lockfile, NODE_MODULES_SEGMENT, PACKAGE_LOCK_FILE_NAME, ROOT_PACKAGE_KEY,
    installed_package, parse_lockfile,
};
use crate::context::ContextAnswer;
use crate::manifest::{
    StaticFileFailure, WorkspacePaths, file_beside, is_ancestor_directory, manifest_directory,
    manifest_directory_path, read_static_file,
};
use crate::node::{
    NPM_MANAGER, PACKAGE_MANIFEST_FILE_NAME, PackageManifest, installed_folder,
    is_workspace_version, parse_package_manifest, path_availability, version_availability,
};
use crate::resolver::{ContextRequest, StaticInputs};

/// The registry URL prefix a package fetched from the public npm registry resolves from.
const NPM_REGISTRY_URL: &str = "https://registry.npmjs.org/";
/// The locator prefix of a package installed from a path relative to the lockfile.
const FILE_LOCATOR_PREFIX: &str = "file:";
/// The prefix of a plain `http:` URL version text or locator.
const HTTP_URL_PREFIX: &str = "http:";
/// The prefix of an `https:` URL version text or locator.
const HTTPS_URL_PREFIX: &str = "https:";
/// The locator prefixes of a package fetched from a git repository.
const GIT_LOCATOR_PREFIXES: [&str; 2] = ["git+", "git:"];

/// Reports what each listed manifest's `package-lock.json` pins and what the
/// `package.json` itself declares. Each package the lockfile installs records its
/// install folder, the key the lockfile files it under, so a nested copy at another
/// version gets a folder of its own.
///
/// This resolver owns the `package.json` read for both npm and Bun, which claim the same
/// manifest: a requirement for a package either lockfile pins is dropped where the
/// answers merge, so one selector reaches the context per package. Each manifest and
/// each lockfile beside one is an input. An absent file is the manifest-only case, not a
/// degradation; a file over its bound or unparsable is one, naming the path.
///
/// The lockfiles and manifests are read first: a package any lockfile links, and a
/// member any `workspaces` glob names, is declared by path, so a member's `"api": "*"`
/// names the workspace's own package, not the registry's.
pub(super) fn npm_context(
    request: &ContextRequest<'_>,
    inputs: &mut dyn StaticInputs,
) -> ContextAnswer {
    let mut answer = ContextAnswer::default();
    let workspace = WorkspacePaths::new(request.root, inputs);
    let mut links = BTreeMap::new();
    let mut parsed = Vec::new();
    for manifest in request.manifests {
        answer.inputs.push(manifest.clone());
        pin_lockfile(
            request.root,
            manifest,
            &workspace,
            inputs,
            &mut links,
            &mut answer,
        );
        let directory = manifest_directory_path(request.root, manifest);
        let observed = read_static_file(&directory, PACKAGE_MANIFEST_FILE_NAME, inputs);
        match observed.and_then(|bytes| parse_package_manifest(&bytes)) {
            Ok(package) => parsed.push((manifest, package)),
            Err(failure) => report(&mut answer, manifest, &failure),
        }
    }
    link_members(request.root, &parsed, &mut links, &mut answer);
    let claimed_names: BTreeSet<&str> = parsed
        .iter()
        .filter_map(|(_, package)| package.name())
        .collect();
    for (manifest, package) in &parsed {
        let directory = manifest_directory_path(request.root, manifest);
        let declared = package.declared(|name, version| match links.get(name) {
            Some(true) => None,
            Some(false) => Some(PackageAvailability::Path),
            None if is_workspace_version(version) && claimed_names.contains(name) => None,
            None => version_availability(version, &directory, &workspace, inputs),
        });
        answer.entries.extend(declared);
    }
    answer
}

/// Records each claimed manifest a `workspaces` glob of another names as a package
/// linked inside the root: npm links every member into `node_modules`, lockfile or not.
///
/// A link a lockfile recorded stands. Every manifest pair is compared, so the work is
/// quadratic in the manifest count, which `MANIFESTS_MAX` bounds.
fn link_members(
    root: &Path,
    parsed: &[(&ProjectPath, PackageManifest)],
    links: &mut BTreeMap<String, bool>,
    answer: &mut ContextAnswer,
) {
    for (manifest, package) in parsed {
        let directory = manifest_directory(manifest);
        let globs = match package.workspace_globs(&manifest_directory_path(root, manifest)) {
            Ok(Some(globs)) => globs,
            Ok(None) => continue,
            Err(error) => {
                let manifest_path = &manifest.0;
                answer.degradations.push(format!(
                    "{manifest_path}: its workspaces globs could not be parsed: {error}; no \
                     members read"
                ));
                continue;
            }
        };
        for (member, member_package) in parsed {
            let member_directory = manifest_directory(member);
            let named = is_ancestor_directory(directory, member_directory)
                && globs
                    .matched(manifest_directory_path(root, member), true)
                    .is_whitelist();
            if named {
                links
                    .entry(member_package.member_name(member_directory))
                    .or_insert(true);
            }
        }
    }
}

/// Reports every package the `package-lock.json` beside one manifest pins, and records
/// each package it links by whether the link target lies inside the root.
fn pin_lockfile(
    root: &Path,
    manifest: &ProjectPath,
    workspace: &WorkspacePaths,
    inputs: &mut dyn StaticInputs,
    links: &mut BTreeMap<String, bool>,
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
    for (key, package) in &lockfile.packages {
        if let Some((name, target)) = hoisted_link(key, package) {
            let inside = workspace.contains(&directory.join(target), inputs);
            links
                .entry(name.to_owned())
                .and_modify(|standing| *standing &= inside)
                .or_insert(inside);
            continue;
        }
        let Some(installed) = installed_package(key, package) else {
            continue;
        };
        let declared_by_url = root_version(&lockfile, installed.name).is_some_and(is_url_version);
        let Some(availability) = resolved_availability(
            package.resolved.as_deref(),
            declared_by_url,
            &directory,
            workspace,
            inputs,
        ) else {
            continue;
        };
        answer.install_folders.push(installed_folder(
            &directory,
            key,
            (installed.name, installed.version),
            inputs,
        ));
        answer.entries.push(PackageContextEntry::new(
            NPM_MANAGER,
            installed.name,
            PackageSelector::Version(installed.version.to_owned()),
            availability,
        ));
    }
}

/// The package name and link target of a hoisted `link` entry: npm links a workspace
/// member, and a `file:` directory, from `node_modules/<name>` to a path relative to
/// the lockfile.
fn hoisted_link<'a>(key: &'a str, package: &'a LockedPackage) -> Option<(&'a str, &'a str)> {
    let name = key.strip_prefix(NODE_MODULES_SEGMENT)?;
    let hoisted = !name.is_empty() && !name.contains(NODE_MODULES_SEGMENT);
    let target = package.resolved.as_deref()?;
    (hoisted && package.link == Some(true)).then_some((name, target))
}

/// The version text the lockfile's root package declares for `name`, across its three
/// dependency maps.
fn root_version<'a>(lockfile: &'a Lockfile, name: &str) -> Option<&'a str> {
    let root = lockfile.packages.get(ROOT_PACKAGE_KEY)?;
    [
        &root.dependencies,
        &root.dev_dependencies,
        &root.optional_dependencies,
    ]
    .into_iter()
    .find_map(|map| map.get(name))
    .map(String::as_str)
}

/// Whether one version text names a tarball URL.
fn is_url_version(version: &str) -> bool {
    version.starts_with(HTTP_URL_PREFIX) || version.starts_with(HTTPS_URL_PREFIX)
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

/// Whether a global package index can answer for the package one lockfile entry
/// resolved from, read from the lockfile in `directory`. `None` for project source.
///
/// An entry with no `resolved` came from the default registry. A `file:` locator is a
/// path relative to the lockfile, and a `git+` or `git:` locator a git repository. Any
/// other `http:` or `https:` host is a `url` entry when the root package declares the
/// package by URL, and a private registry otherwise: the two lock the same shape.
fn resolved_availability(
    resolved: Option<&str>,
    declared_by_url: bool,
    directory: &Path,
    workspace: &WorkspacePaths,
    inputs: &mut dyn StaticInputs,
) -> Option<PackageAvailability> {
    let Some(locator) = resolved else {
        return Some(PackageAvailability::Canonical);
    };
    if locator.starts_with(NPM_REGISTRY_URL) {
        return Some(PackageAvailability::Canonical);
    }
    if let Some(path) = locator.strip_prefix(FILE_LOCATOR_PREFIX) {
        return path_availability(path, directory, workspace, inputs);
    }
    if GIT_LOCATOR_PREFIXES
        .iter()
        .any(|prefix| locator.starts_with(prefix))
    {
        return Some(PackageAvailability::Git);
    }
    if declared_by_url && is_url_version(locator) {
        return Some(PackageAvailability::Url);
    }
    Some(PackageAvailability::PrivateRegistry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NpmResolver;
    use crate::context::InstallLocation;
    use crate::fixture::RecordedInspector;
    use crate::resolver::{DependencyResolver, LOCKFILE_BYTES_MAX};

    const ROOT: &str = "/workspace";

    /// A lockfile pinning one registry package, one private-registry package, one git
    /// package, one bundled package, one nested copy at another version than the top-level
    /// one, and one linked workspace package.
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
    "node_modules/tool": {
      "version": "3.0.0",
      "resolved": "git+ssh://git@github.com/example/tool.git#0123456789abcdef"
    },
    "node_modules/left-pad/node_modules/zod": {
      "version": "3.25.76",
      "resolved": "https://registry.npmjs.org/zod/-/zod-3.25.76.tgz"
    },
    "node_modules/zod": {
      "version": "4.0.0",
      "resolved": "https://registry.npmjs.org/zod/-/zod-4.0.0.tgz"
    },
    "packages/api": { "name": "api", "version": "0.0.1" },
    "node_modules/api": { "resolved": "packages/api", "link": true }
  }
}
"#;

    fn project(path: &str) -> ProjectPath {
        ProjectPath(path.to_owned())
    }

    fn context(manifests: &[&str], inspector: &mut RecordedInspector) -> ContextAnswer {
        context_at(ROOT, manifests, inspector)
    }

    fn context_at(
        root: &str,
        manifests: &[&str],
        inspector: &mut RecordedInspector,
    ) -> ContextAnswer {
        let manifests: Vec<ProjectPath> = manifests.iter().map(|path| project(path)).collect();
        let request = ContextRequest {
            root: Path::new(root),
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
    fn test_a_lockfile_pins_exact_versions_and_sorts_every_locator_by_kind() {
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/package-lock.json"), LOCKFILE)
            .with_canonical(
                format!("{ROOT}/node_modules/left-pad"),
                format!("{ROOT}/node_modules/.pnpm/left-pad@1.3.0/node_modules/left-pad"),
            );

        let answer = context(&["package.json"], &mut inspector);

        assert_eq!(
            reported(&answer),
            [
                (
                    "bundled: version 0.1.0".to_owned(),
                    PackageAvailability::Canonical
                ),
                (
                    "internal-tool: version 2.0.0".to_owned(),
                    PackageAvailability::PrivateRegistry
                ),
                (
                    "left-pad: version 1.3.0".to_owned(),
                    PackageAvailability::Canonical
                ),
                (
                    "zod: version 3.25.76".to_owned(),
                    PackageAvailability::Canonical
                ),
                ("tool: version 3.0.0".to_owned(), PackageAvailability::Git),
                (
                    "zod: version 4.0.0".to_owned(),
                    PackageAvailability::Canonical
                ),
            ],
            "a linked workspace package pins no version of its own"
        );
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
        let at = |path: &str| InstallLocation::Path(Path::new(ROOT).join(path));
        assert_eq!(
            folders,
            [
                ("npm/bundled@0.1.0".to_owned(), &at("node_modules/bundled")),
                (
                    "npm/internal-tool@2.0.0".to_owned(),
                    &at("node_modules/internal-tool")
                ),
                (
                    "npm/left-pad@1.3.0".to_owned(),
                    &at("node_modules/.pnpm/left-pad@1.3.0/node_modules/left-pad")
                ),
                (
                    "npm/zod@3.25.76".to_owned(),
                    &at("node_modules/left-pad/node_modules/zod")
                ),
                ("npm/tool@3.0.0".to_owned(), &at("node_modules/tool")),
                ("npm/zod@4.0.0".to_owned(), &at("node_modules/zod")),
            ],
            "a nested copy at another version gets a folder of its own, and a linked \
             folder is recorded where the link resolves"
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
                "cataloged: requirement >=0",
                "left-pad: requirement ^1.3.0",
                "local: requirement file:../local",
                "pinned: version 1.3.0",
                "shorthand: requirement user/repo#main",
                "sourced: requirement git+https://example.test/tool.git",
                "typescript: version 5.9.3",
                "fsevents: requirement ~2.3.0"
            ],
            "an `npm:` value names the package it aliases, and a catalog range the pass \
             does not read goes out as `>=0`"
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
                ("shorthand", PackageAvailability::Git),
                ("sourced", PackageAvailability::Git)
            ],
            "a repository shorthand names a git repository"
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

    /// `(name, selector, availability)` per entry, in answer order.
    fn reported(answer: &ContextAnswer) -> Vec<(String, PackageAvailability)> {
        spelled(answer)
            .into_iter()
            .zip(answer.entries.iter().map(|entry| entry.availability))
            .collect()
    }

    /// The root `package.json` of a workspace npm 11.5.1 installs: one member, one
    /// directory inside the root, one outside it, and one tarball outside it.
    const WORKSPACE_MANIFEST: &str = r#"{"name":"probe","private":true,"workspaces":["packages/*"],
 "dependencies":{"api":"*","local":"file:libs/local","outside":"file:../outside","tarred":"file:../tarred-2.0.0.tgz"}}
"#;

    /// `npm install --package-lock-only` over [`WORKSPACE_MANIFEST`]: the member and both
    /// directories lock as links.
    const LINKED_LOCKFILE: &str = r#"{
  "name": "probe",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "probe",
      "workspaces": ["packages/*"],
      "dependencies": {
        "api": "*",
        "local": "file:libs/local",
        "outside": "file:../outside",
        "tarred": "file:../tarred-2.0.0.tgz"
      }
    },
    "../outside": { "version": "1.0.0" },
    "libs/local": { "version": "0.3.0" },
    "node_modules/api": { "resolved": "packages/api", "link": true },
    "node_modules/local": { "resolved": "libs/local", "link": true },
    "node_modules/outside": { "resolved": "../outside", "link": true },
    "node_modules/tarred": {
      "version": "2.0.0",
      "resolved": "file:../tarred-2.0.0.tgz",
      "integrity": "sha512-GgsfkRwARu8/7Sli6UJBPygceTetBSA9b65LIaOZMr95q4tB6YaNl+IlXSV00A0CBvvr8KlU2OZ8id1EdtyX/A=="
    },
    "packages/api": { "version": "0.0.1" }
  }
}
"#;

    /// The same install with `--install-links`: both directories lock as copies with a
    /// `file:` locator; the member stays a link.
    const COPIED_LOCKFILE: &str = r#"{
  "name": "probe",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "probe",
      "workspaces": ["packages/*"],
      "dependencies": {
        "api": "*",
        "local": "file:libs/local",
        "outside": "file:../outside",
        "tarred": "file:../tarred-2.0.0.tgz"
      }
    },
    "node_modules/api": { "resolved": "packages/api", "link": true },
    "node_modules/local": { "version": "0.3.0", "resolved": "file:libs/local" },
    "node_modules/outside": { "version": "1.0.0", "resolved": "file:../outside" },
    "node_modules/tarred": {
      "version": "2.0.0",
      "resolved": "file:../tarred-2.0.0.tgz",
      "integrity": "sha512-GgsfkRwARu8/7Sli6UJBPygceTetBSA9b65LIaOZMr95q4tB6YaNl+IlXSV00A0CBvvr8KlU2OZ8id1EdtyX/A=="
    },
    "packages/api": { "version": "0.0.1" }
  }
}
"#;

    #[test]
    fn test_a_linked_member_and_a_directory_inside_the_root_are_project_source() {
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/package.json"), WORKSPACE_MANIFEST)
            .with_file(format!("{ROOT}/package-lock.json"), LINKED_LOCKFILE)
            .with_file(
                format!("{ROOT}/packages/api/package.json"),
                r#"{"name":"api","version":"0.0.1","dependencies":{"local":"^0.3.0"}}"#,
            );

        let answer = context(
            &["package.json", "packages/api/package.json"],
            &mut inspector,
        );

        assert_eq!(
            reported(&answer),
            [
                (
                    "tarred: version 2.0.0".to_owned(),
                    PackageAvailability::Path
                ),
                (
                    "outside: requirement file:../outside".to_owned(),
                    PackageAvailability::Path
                ),
                (
                    "tarred: requirement file:../tarred-2.0.0.tgz".to_owned(),
                    PackageAvailability::Path
                ),
            ],
            "`api` and `local` link inside the root, so neither `*` nor `^0.3.0` reaches the \
             registry; `outside` links out, and a tarball is no project source"
        );
    }

    #[test]
    fn test_an_installed_copy_inside_the_root_is_project_source() {
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/package.json"), WORKSPACE_MANIFEST)
            .with_file(format!("{ROOT}/package-lock.json"), COPIED_LOCKFILE);

        let answer = context(&["package.json"], &mut inspector);

        assert_eq!(
            reported(&answer),
            [
                (
                    "outside: version 1.0.0".to_owned(),
                    PackageAvailability::Path
                ),
                (
                    "tarred: version 2.0.0".to_owned(),
                    PackageAvailability::Path
                ),
                (
                    "outside: requirement file:../outside".to_owned(),
                    PackageAvailability::Path
                ),
                (
                    "tarred: requirement file:../tarred-2.0.0.tgz".to_owned(),
                    PackageAvailability::Path
                ),
            ]
        );
    }

    #[test]
    fn test_workspace_and_link_versions_follow_the_resolved_root() {
        // The bun workspace at ~/projects/schorle declares its members both ways:
        // `workspace:*` from a member, `link:<name>` from the root, where bun's `link:`
        // names a globally linked package and nothing stands at the path.
        let manifest = r#"{"name":"schorle-mono","workspaces":["packages/*"],
 "devDependencies":{"@schorle/server":"link:@schorle/server","vendored":"link:vendor/vendored"}}"#;
        let member = r#"{"name":"aurora","dependencies":{"@schorle/shared":"workspace:*"}}"#;
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/package.json"), manifest)
            .with_file(format!("{ROOT}/packages/aurora/package.json"), member)
            .with_file(
                format!("{ROOT}/packages/shared/package.json"),
                r#"{"name":"@schorle/shared"}"#,
            )
            .with_canonical(ROOT, ROOT)
            .with_canonical(
                format!("{ROOT}/vendor/vendored"),
                format!("{ROOT}/vendor/vendored"),
            );

        let answer = context(
            &[
                "package.json",
                "packages/aurora/package.json",
                "packages/shared/package.json",
            ],
            &mut inspector,
        );

        assert_eq!(
            reported(&answer),
            [(
                "@schorle/server: requirement link:@schorle/server".to_owned(),
                PackageAvailability::Path
            )],
            "a link to a directory inside the root and a workspace member are project \
             source; a link where nothing stands is not"
        );
    }

    #[test]
    fn test_an_archive_inside_the_root_is_a_path_entry() {
        let manifest = r#"{"dependencies":{"local":"file:libs/local","packed":"file:vendor/packed-1.0.0.tgz"}}"#;
        let mut inspector =
            RecordedInspector::default().with_file(format!("{ROOT}/package.json"), manifest);

        let answer = context(&["package.json"], &mut inspector);

        assert_eq!(
            reported(&answer),
            [(
                "packed: requirement file:vendor/packed-1.0.0.tgz".to_owned(),
                PackageAvailability::Path
            )],
            "a directory inside the root is project source; an archive inside it is not, \
             since the local index reads no archive"
        );
    }

    #[test]
    fn test_a_tarball_url_is_a_url_entry() {
        let manifest = r#"{"dependencies":{"tarball":"https://example.test/tarball-1.0.0.tgz"}}"#;
        let lockfile = r#"{
  "lockfileVersion": 3,
  "packages": {
    "": { "dependencies": { "tarball": "https://example.test/tarball-1.0.0.tgz" } },
    "node_modules/tarball": {
      "version": "1.0.0",
      "resolved": "https://example.test/tarball-1.0.0.tgz"
    },
    "node_modules/internal-tool": {
      "version": "2.0.0",
      "resolved": "https://npm.example.test/internal-tool/-/internal-tool-2.0.0.tgz"
    }
  }
}"#;
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/package.json"), manifest)
            .with_file(format!("{ROOT}/package-lock.json"), lockfile);

        let answer = context(&["package.json"], &mut inspector);

        assert_eq!(
            reported(&answer),
            [
                (
                    "internal-tool: version 2.0.0".to_owned(),
                    PackageAvailability::PrivateRegistry
                ),
                (
                    "tarball: version 1.0.0".to_owned(),
                    PackageAvailability::Url
                ),
                (
                    "tarball: requirement https://example.test/tarball-1.0.0.tgz".to_owned(),
                    PackageAvailability::Url
                ),
            ],
            "a host the root declares no URL for locks like a private registry"
        );
    }

    #[test]
    fn test_a_workspace_version_is_project_source_only_when_a_claimed_manifest_declares_it() {
        // A pnpm monorepo: `pnpm-workspace.yaml` names the members, so no `package.json`
        // carries `workspaces`. `plugin` stands below `packages/app`; `shared` beside it.
        let app = r#"{"name":"app","dependencies":{"shared":"workspace:*","plugin":"workspace:^","left-pad":"^1.3.0"}}"#;
        let files = [
            ("/mono/package.json", r#"{"name":"mono","private":true}"#),
            (
                "/mono/pnpm-workspace.yaml",
                "packages:\n  - packages/*\n  - packages/app/plugins/*\n",
            ),
            ("/mono/packages/app/package.json", app),
            (
                "/mono/packages/app/plugins/plugin/package.json",
                r#"{"name":"plugin","version":"0.1.0"}"#,
            ),
            (
                "/mono/packages/shared/package.json",
                r#"{"name":"shared","version":"0.1.0"}"#,
            ),
        ];
        let inspector = || {
            files
                .iter()
                .fold(RecordedInspector::default(), |inspector, (path, text)| {
                    inspector.with_file(*path, *text)
                })
        };

        let below = context_at(
            "/mono/packages/app",
            &["package.json", "plugins/plugin/package.json"],
            &mut inspector(),
        );
        let whole = context_at(
            "/mono",
            &[
                "package.json",
                "packages/app/package.json",
                "packages/app/plugins/plugin/package.json",
                "packages/shared/package.json",
            ],
            &mut inspector(),
        );

        assert_eq!(
            reported(&below),
            [
                (
                    "left-pad: requirement ^1.3.0".to_owned(),
                    PackageAvailability::Canonical
                ),
                (
                    "shared: requirement workspace:*".to_owned(),
                    PackageAvailability::Path
                ),
            ],
            "served at `packages/app`, `shared` stands outside the root and `plugin` inside"
        );
        assert_eq!(
            reported(&whole),
            [(
                "left-pad: requirement ^1.3.0".to_owned(),
                PackageAvailability::Canonical
            )]
        );
    }

    /// The root `package.json` of a workspace npm 11.5.1 installs: the `workspaces` table
    /// form, a `./` glob, and a negated member.
    const TABLE_WORKSPACE_MANIFEST: &str = r#"{"name":"probe","private":true,"workspaces":{"packages":["./packages/*","!packages/legacy"]},"dependencies":{"api":"*"}}"#;

    /// The lockfile npm 11.5.1 writes for `npm install --package-lock-only --offline` over
    /// [`TABLE_WORKSPACE_MANIFEST`]: `legacy` and `examples/demo` are no members.
    const TABLE_WORKSPACE_LOCKFILE: &str = r#"{
  "name": "probe",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "probe",
      "dependencies": { "api": "*" },
      "workspaces": { "packages": ["./packages/*", "!packages/legacy"] }
    },
    "node_modules/api": { "resolved": "packages/api", "link": true },
    "node_modules/web": { "resolved": "packages/web", "link": true },
    "packages/api": { "version": "0.0.1" },
    "packages/web": { "version": "0.0.1", "dependencies": { "api": "*" } }
  }
}
"#;

    /// The table-form workspace's files below `ROOT`, the root manifest given.
    fn table_workspace(root_manifest: &str) -> RecordedInspector {
        RecordedInspector::default()
            .with_file(format!("{ROOT}/package.json"), root_manifest)
            .with_file(
                format!("{ROOT}/packages/api/package.json"),
                r#"{"name":"api","version":"0.0.1"}"#,
            )
            .with_file(
                format!("{ROOT}/packages/web/package.json"),
                r#"{"name":"web","version":"0.0.1","dependencies":{"api":"*"}}"#,
            )
            .with_file(
                format!("{ROOT}/packages/legacy/package.json"),
                r#"{"name":"legacy","version":"0.0.1"}"#,
            )
            .with_file(
                format!("{ROOT}/packages/tools/package.json"),
                r#"{"version":"1.0.0"}"#,
            )
            .with_file(
                format!("{ROOT}/examples/demo/package.json"),
                r#"{"name":"demo","version":"0.0.1"}"#,
            )
    }

    const TABLE_WORKSPACE_MANIFESTS: [&str; 6] = [
        "examples/demo/package.json",
        "package.json",
        "packages/api/package.json",
        "packages/legacy/package.json",
        "packages/tools/package.json",
        "packages/web/package.json",
    ];

    #[test]
    fn test_a_member_is_left_out_with_a_lockfile_and_without() {
        let mut locked = table_workspace(TABLE_WORKSPACE_MANIFEST).with_file(
            format!("{ROOT}/package-lock.json"),
            TABLE_WORKSPACE_LOCKFILE,
        );
        let mut unlocked = table_workspace(TABLE_WORKSPACE_MANIFEST);

        let locked = context(&TABLE_WORKSPACE_MANIFESTS, &mut locked);
        let unlocked = context(&TABLE_WORKSPACE_MANIFESTS, &mut unlocked);

        assert!(
            locked.entries.is_empty(),
            "the lockfile links `api` inside the root: {:?}",
            reported(&locked)
        );
        assert!(
            unlocked.entries.is_empty(),
            "the `workspaces` globs name `api` a member, so neither `*` reaches the \
             registry: {:?}",
            reported(&unlocked)
        );
        assert!(locked.degradations.is_empty() && unlocked.degradations.is_empty());
    }

    #[test]
    fn test_workspaces_globs_name_members_as_npm_matches_them() {
        let manifest = r#"{"name":"probe","private":true,"workspaces":{"packages":["./packages/*","!packages/legacy"]},
 "dependencies":{"api":"*","demo":"^1.0.0","legacy":"^2.0.0","tools":"*"}}"#;
        let mut inspector = table_workspace(manifest);

        let answer = context(&TABLE_WORKSPACE_MANIFESTS, &mut inspector);

        assert_eq!(
            reported(&answer),
            [
                (
                    "demo: requirement ^1.0.0".to_owned(),
                    PackageAvailability::Canonical
                ),
                (
                    "legacy: requirement ^2.0.0".to_owned(),
                    PackageAvailability::Canonical
                ),
            ],
            "a member with no `name` takes its folder name; a negated folder and one no glob \
             names are no members"
        );
    }

    /// An empty `workspaces` list names no member, so a package declared under a member's
    /// name is a registry package, and nothing degrades.
    #[test]
    fn test_an_empty_workspaces_list_names_no_member() {
        let manifest = r#"{"name":"probe","workspaces":[],"dependencies":{"api":"*"}}"#;
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/package.json"), manifest)
            .with_file(
                format!("{ROOT}/packages/api/package.json"),
                r#"{"name":"api"}"#,
            );

        let answer = context(
            &["package.json", "packages/api/package.json"],
            &mut inspector,
        );

        assert_eq!(
            reported(&answer),
            [(
                "api: requirement *".to_owned(),
                PackageAvailability::Canonical
            )]
        );
        assert!(answer.degradations.is_empty());
    }

    #[test]
    fn test_an_invalid_workspaces_glob_is_a_degradation_and_the_declarations_stand() {
        let manifest = r#"{"name":"probe","workspaces":["packages/["],"dependencies":{"api":"*"}}"#;
        let mut inspector = RecordedInspector::default()
            .with_file(format!("{ROOT}/package.json"), manifest)
            .with_file(
                format!("{ROOT}/packages/api/package.json"),
                r#"{"name":"api"}"#,
            );

        let answer = context(
            &["package.json", "packages/api/package.json"],
            &mut inspector,
        );

        assert_eq!(
            reported(&answer),
            [(
                "api: requirement *".to_owned(),
                PackageAvailability::Canonical
            )]
        );
        assert_eq!(answer.degradations.len(), 1);
        assert!(
            answer.degradations[0]
                .starts_with("package.json: its workspaces globs could not be parsed: "),
            "{:?}",
            answer.degradations[0]
        );
    }
}
