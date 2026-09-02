//! Test fixtures shared by the uv resolver's lockfile and environment suites.

use std::path::Path;

use rift_protocol::read::ProjectPath;

use super::{UV_LOCK_FILE_NAME, UvResolver};
use crate::catalog::{CatalogEntry, Resolution};
use crate::fixture::RecordedInspector;
use crate::resolver::{DependencyResolver as _, ResolutionRequest};

pub(super) const ROOT: &str = "/workspace";
pub(super) const ENVIRONMENT: &str = "/workspace/.venv";
pub(super) const SITE_PACKAGES: &str = "/workspace/.venv/lib/python3.14t/site-packages";
pub(super) const INTERPRETER_PREFIX: &str = "/toolchain/cpython-3.14.0";
pub(super) const STDLIB_DIRECTORY: &str = "/toolchain/cpython-3.14.0/lib/python3.14t";
pub(super) const ENVIRONMENT_FILE: &str = "home = /toolchain/cpython-3.14.0/bin\n\
    implementation = CPython\n\
    uv = 0.9.5\n\
    version_info = 3.14.0\n\
    include-system-site-packages = false\n\
    prompt = rift-release\n";
pub(super) const REGISTRY: &str = r#"{ registry = "https://pypi.org/simple" }"#;

/// Registry packages, an editable member, and a virtual member with a dynamic version.
pub(super) const WORKSPACE_LOCKFILE: &str = r#"
version = 1
revision = 3
requires-python = ">=3.10"

[manifest]
members = ["rift-release", "tools"]

[[package]]
name = "colorama"
version = "0.4.6"
source = { registry = "https://pypi.org/simple" }
sdist = { url = "https://files.pythonhosted.org/packages/colorama-0.4.6.tar.gz", hash = "sha256:0000", size = 27697 }

[[package]]
name = "markdown-it-py"
version = "4.2.0"
source = { registry = "https://pypi.org/simple" }
dependencies = [
    { name = "mdurl" },
]

[[package]]
name = "mdurl"
version = "0.1.2"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "pluggy"
version = "1.6.0"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "py"
version = "1.11.0"
source = { registry = "https://pypi.org/simple" }

[[package]]
name = "pytest"
version = "9.1.1"
source = { registry = "https://pypi.org/simple" }
dependencies = [
    { name = "colorama", marker = "sys_platform == 'win32'" },
    { name = "pluggy" },
]

[[package]]
name = "rift-release"
version = "0.0.1"
source = { editable = "." }
dependencies = [
    { name = "markdown-it-py" },
    { name = "typer" },
]

[package.dev-dependencies]
dev = [
    { name = "pytest" },
]

[package.metadata]
requires-dist = [{ name = "typer", specifier = ">=0.20.0,<1" }]

[package.metadata.requires-dev]
dev = [{ name = "pytest", specifier = ">=8.4.0,<10" }]

[[package]]
name = "tools"
source = { virtual = "tools" }
dependencies = [
    { name = "colorama" },
]

[[package]]
name = "typer"
version = "0.27.1"
source = { registry = "https://pypi.org/simple" }
"#;

pub(super) fn project(path: &str) -> ProjectPath {
    ProjectPath(path.to_owned())
}

pub(super) fn resolve(manifests: &[&str], inspector: &mut RecordedInspector) -> Resolution {
    let manifests: Vec<ProjectPath> = manifests.iter().map(|path| project(path)).collect();
    let request = ResolutionRequest {
        root: Path::new(ROOT),
        manifests: &manifests,
    };
    UvResolver::new().resolve(&request, inspector)
}

/// Scripts a POSIX-layout environment at `environment` holding the workspace's distributions.
pub(super) fn with_environment(
    inspector: RecordedInspector,
    environment: &str,
) -> RecordedInspector {
    let site_packages = format!("{environment}/lib/python3.14t/site-packages");
    inspector
        .with_file(format!("{environment}/pyvenv.cfg"), ENVIRONMENT_FILE)
        .with_file(
            format!("{site_packages}/pytest-9.1.1.dist-info/top_level.txt"),
            "_pytest\npy\npytest\n",
        )
        .with_file(
            format!("{site_packages}/typer-0.27.1.dist-info/top_level.txt"),
            "typer\n",
        )
        .with_directory(format!("{site_packages}/markdown_it_py-4.2.0.dist-info"))
        .with_directory(format!("{site_packages}/mdurl-0.1.2.dist-info"))
        .with_directory(format!("{site_packages}/py-1.11.0.dist-info"))
        .with_directory(format!("{site_packages}/_pytest"))
        .with_directory(format!("{site_packages}/colorama"))
        .with_directory(format!("{site_packages}/markdown_it"))
        .with_directory(format!("{site_packages}/mdurl"))
        .with_directory(format!("{site_packages}/pytest"))
        .with_directory(format!("{site_packages}/typer"))
        .with_file(format!("{site_packages}/py.py"), "")
        .with_directory(STDLIB_DIRECTORY)
}

/// The workspace lockfile at the root and the environment `uv sync` installed beside it.
pub(super) fn environment_inspector() -> RecordedInspector {
    let inspector =
        RecordedInspector::default().with_file(format!("{ROOT}/uv.lock"), WORKSPACE_LOCKFILE);
    with_environment(inspector, ENVIRONMENT)
}

/// One registry package and one editable member depending on it, as one lockfile.
pub(super) fn single_package_lockfile(name: &str, version: &str) -> String {
    format!(
        "[[package]]\nname = \"{name}\"\nversion = \"{version}\"\nsource = {REGISTRY}\n\n\
         [[package]]\nname = \"app\"\nversion = \"0.1.0\"\nsource = {{ editable = \".\" }}\n\
         dependencies = [\n    {{ name = \"{name}\" }},\n]\n"
    )
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

pub(super) fn lockfile_reads(inspector: &RecordedInspector) -> Vec<&str> {
    inspector
        .asked
        .iter()
        .filter(|line| line.starts_with("read ") && line.ends_with(UV_LOCK_FILE_NAME))
        .map(String::as_str)
        .collect()
}

pub(super) fn asked_count(inspector: &RecordedInspector, line: &str) -> usize {
    inspector
        .asked
        .iter()
        .filter(|asked| *asked == line)
        .count()
}
