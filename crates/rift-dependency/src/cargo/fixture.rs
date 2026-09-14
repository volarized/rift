//! Test fixtures shared by the Cargo resolver's metadata, lockfile, and toolchain suites.

use std::path::Path;

use rift_protocol::read::ProjectPath;
use serde_json::json;

use super::toolchain::SYSROOT_LIBRARY_PATH;
use super::{CARGO_LOCK_FILE_NAME, CargoResolver};
use crate::catalog::{CatalogEntry, Resolution};
use crate::fixture::RecordedInspector;
use crate::resolver::{DependencyResolver as _, ResolutionRequest};

pub(super) const ROOT: &str = "/workspace";
pub(super) const CARGO_HOME: &str = "/cargo-home";
pub(super) const SYSROOT: &str = "/toolchain";
pub(super) const METADATA_RENDERED: &str = "cargo metadata --format-version 1 --locked --offline";
pub(super) const REGISTRY_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";
pub(super) const INDEX_DIRECTORY: &str = "index.crates.io-1949cf8c6b5b557f";
pub(super) const RUFF_REVISION: &str = "2b0d21094e2a55491bff60c07fd6f8803876cae5";
pub(super) const RUFF_SOURCE: &str = "git+https://github.com/astral-sh/ruff?rev=2b0d21094e2a55491bff60c07fd6f8803876cae5#2b0d21094e2a55491bff60c07fd6f8803876cae5";

pub(super) fn project(path: &str) -> ProjectPath {
    ProjectPath(path.to_owned())
}

pub(super) fn resolve(manifests: &[&str], inspector: &mut RecordedInspector) -> Resolution {
    let manifests: Vec<ProjectPath> = manifests.iter().map(|path| project(path)).collect();
    let request = ResolutionRequest {
        root: Path::new(ROOT),
        manifests: &manifests,
    };
    CargoResolver::new().resolve(&request, inspector)
}

pub(super) fn with_rustc(inspector: RecordedInspector) -> RecordedInspector {
    inspector
        .with_command(
            "rustc --print sysroot",
            RecordedInspector::succeeded(format!("{SYSROOT}\n")),
        )
        .with_command(
            "rustc --version",
            RecordedInspector::succeeded("rustc 1.98.0 (88d9e12ae 2026-08-18)\n"),
        )
        .with_directory(format!("{SYSROOT}/{SYSROOT_LIBRARY_PATH}"))
}

pub(super) fn registry_id(name: &str, version: &str) -> String {
    format!("{REGISTRY_SOURCE}#{name}@{version}")
}

pub(super) fn registry_manifest(name: &str, version: &str) -> String {
    format!("{CARGO_HOME}/registry/src/{INDEX_DIRECTORY}/{name}-{version}/Cargo.toml")
}

pub(super) fn package(
    name: &str,
    version: &str,
    id: &str,
    manifest_path: &str,
) -> serde_json::Value {
    json!({
        "name": name,
        "version": version,
        "id": id,
        "source": if id.starts_with("path+") { serde_json::Value::Null } else { json!(REGISTRY_SOURCE) },
        "manifest_path": manifest_path,
        "dependencies": [],
        "targets": [],
    })
}

fn node(id: &str, dependencies: &[&str]) -> serde_json::Value {
    json!({
        "id": id,
        "deps": dependencies.iter().map(|pkg| json!({ "pkg": pkg, "name": "x" })).collect::<Vec<_>>(),
        "dependencies": dependencies,
        "features": [],
    })
}

/// Two members, one direct registry package, one transitive registry package, one git
/// package, and one path package outside the members.
pub(super) fn workspace_metadata() -> String {
    let member_a = format!("path+file://{ROOT}/crates/a#0.1.0");
    let member_b = format!("path+file://{ROOT}/crates/b#0.1.0");
    let serde = registry_id("serde", "1.0.228");
    let serde_derive = registry_id("serde_derive", "1.0.228");
    let ty_project =
        format!("git+https://github.com/astral-sh/ruff?rev={RUFF_REVISION}#ty_project@0.0.0");
    let local_tool = "path+file:///elsewhere/local-tool#0.3.0";
    json!({
        "packages": [
            package("a", "0.1.0", &member_a, &format!("{ROOT}/crates/a/Cargo.toml")),
            package("b", "0.1.0", &member_b, &format!("{ROOT}/crates/b/Cargo.toml")),
            package("serde", "1.0.228", &serde, &registry_manifest("serde", "1.0.228")),
            package("serde_derive", "1.0.228", &serde_derive, &registry_manifest("serde_derive", "1.0.228")),
            package("ty_project", "0.0.0", &ty_project, &format!("{CARGO_HOME}/git/checkouts/ruff-b18f69e2b025fac7/2b0d210/crates/ty_project/Cargo.toml")),
            package("local-tool", "0.3.0", local_tool, "/elsewhere/local-tool/Cargo.toml"),
        ],
        "workspace_members": [member_a, member_b],
        "resolve": {
            "nodes": [
                node(&member_a, &[&serde, local_tool]),
                node(&member_b, &[&ty_project]),
                node(&serde, &[&serde_derive]),
                node(&serde_derive, &[]),
                node(&ty_project, &[]),
                node(local_tool, &[]),
            ],
            "root": null,
        },
        "workspace_root": ROOT,
        "version": 1,
    })
    .to_string()
}

pub(super) const WORKSPACE_LOCKFILE: &str = r#"
version = 4

[[package]]
name = "app"
version = "0.1.0"
dependencies = [
 "itertools 0.15.0",
 "serde",
 "ty_project",
]

[[package]]
name = "itertools"
version = "0.15.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000"

[[package]]
name = "serde"
version = "1.0.228"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000"
dependencies = [
 "serde_derive",
]

[[package]]
name = "serde_derive"
version = "1.0.228"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000"

[[package]]
name = "ty_project"
version = "0.0.0"
source = "git+https://github.com/astral-sh/ruff?rev=2b0d21094e2a55491bff60c07fd6f8803876cae5#2b0d21094e2a55491bff60c07fd6f8803876cae5"
"#;

pub(super) fn lockfile_inspector() -> RecordedInspector {
    RecordedInspector::default()
        .with_file(format!("{ROOT}/Cargo.lock"), WORKSPACE_LOCKFILE)
        .with_environment("CARGO_HOME", CARGO_HOME)
        .with_directory(format!(
            "{CARGO_HOME}/registry/src/{INDEX_DIRECTORY}/serde-1.0.228"
        ))
        .with_directory(format!(
            "{CARGO_HOME}/registry/src/{INDEX_DIRECTORY}/itertools-0.15.0"
        ))
        .with_directory(format!(
            "{CARGO_HOME}/git/checkouts/ruff-b18f69e2b025fac7/2b0d210"
        ))
}

pub(super) fn names(resolution: &Resolution) -> Vec<String> {
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

pub(super) fn entry<'a>(resolution: &'a Resolution, name: &str) -> &'a CatalogEntry {
    resolution
        .entries
        .iter()
        .find(|entry| entry.identity().name == name)
        .expect("entry is cataloged")
}

pub(super) fn metadata_runs(inspector: &RecordedInspector) -> Vec<&str> {
    inspector
        .asked
        .iter()
        .filter(|line| line.starts_with(&format!("run {METADATA_RENDERED} in ")))
        .map(String::as_str)
        .collect()
}

pub(super) fn read_lockfile_count(inspector: &RecordedInspector) -> usize {
    inspector
        .asked
        .iter()
        .filter(|line| line.starts_with("read ") && line.ends_with(CARGO_LOCK_FILE_NAME))
        .count()
}
