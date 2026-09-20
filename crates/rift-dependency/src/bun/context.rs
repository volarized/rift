//! The Bun static context: the packages `bun.lock` pins.

use std::path::Path;

use rift_protocol::dependencies::PackageContextEntry;
use rift_protocol::read::ProjectPath;

use super::{BUN_LOCK_FILE_NAME, Pin, parse_lockfile};
use crate::context::ContextAnswer;
use crate::manifest::{file_beside, manifest_directory_path, read_static_file};
use crate::node::{NPM_MANAGER, npm_selector, version_availability};
use crate::resolver::{ContextRequest, StaticInputs};

/// Reports the packages every `bun.lock` beside a listed manifest pins.
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
    for manifest in request.manifests {
        pin_lockfile(request.root, manifest, inputs, &mut answer);
    }
    answer
}

/// Reports every package the `bun.lock` beside one manifest pins.
fn pin_lockfile(
    root: &Path,
    manifest: &ProjectPath,
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
    let pinned = lockfile
        .packages
        .values()
        .filter_map(|package| match package.pin() {
            Pin::Package { name, version, .. } => Some(PackageContextEntry::new(
                NPM_MANAGER,
                name,
                npm_selector(version),
                version_availability(version),
            )),
            Pin::Malformed | Pin::Workspace => None,
        });
    answer.entries.extend(pinned);
}

#[cfg(test)]
mod tests {
    use rift_protocol::dependencies::PackageAvailability;

    use super::*;
    use crate::BunResolver;
    use crate::fixture::RecordedInspector;
    use crate::resolver::{DependencyResolver, LOCKFILE_BYTES_MAX};

    const ROOT: &str = "/workspace";

    /// A lockfile pinning one registry package, one repository package, and the
    /// workspace's own member.
    const LOCKFILE: &str = r#"{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "probe", "devDependencies": { "typescript": "5.9.3" } },
  },
  "packages": {
    "typescript": ["typescript@5.9.3", "", {}, "sha512-jl1"],
    "tool": ["tool@git+https://example.test/tool.git#abc1234", "", {}, ""],
    "api": ["api@workspace:packages/api"],
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
    fn test_a_lockfile_pins_exact_versions_and_marks_local_only_references() {
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
                "tool: requirement git+https://example.test/tool.git#abc1234 (LocalOnly)",
                "typescript: version 5.9.3 (Canonical)"
            ],
            "a workspace package and a malformed tuple pin nothing"
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
            answer.entries[1].availability,
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
