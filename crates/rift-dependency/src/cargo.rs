//! The Cargo resolver: Rust packages as `cargo metadata` resolved them, or as `Cargo.lock` states.

#[cfg(test)]
mod fixture;
mod lockfile;
mod toolchain;

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use rift_core::line::{lines_inclusive, without_ending};
use rift_protocol::read::{Language, ProjectPath};
use serde::Deserialize;

use crate::catalog::{CatalogEntry, Resolution, package_identity};
use crate::manifest::{
    ResolutionBuilder, file_beside, manifest_directory_path, top_level_manifests,
};
use crate::resolver::{
    CommandOutput, DependencyResolver, Inspector, ResolutionRequest, ResolverName,
    TOOLCHAIN_OUTPUT_BYTES_MAX, ToolchainCommand,
};

/// The package namespace every Cargo dependency entry belongs to.
const CARGO_MANAGER: &str = "cargo";
/// The Cargo executable, resolved on the inspector's `PATH`.
const CARGO_PROGRAM: &str = "cargo";
/// The manifest file name this resolver claims.
const CARGO_MANIFEST_FILE_NAME: &str = "Cargo.toml";
/// The lockfile Cargo keeps beside a workspace root manifest.
const CARGO_LOCK_FILE_NAME: &str = "Cargo.lock";
/// The `cargo metadata` arguments. `--locked` refuses to write the lockfile and
/// `--offline` refuses the network; both are mandatory for every run.
const METADATA_ARGUMENTS: [&str; 5] =
    ["metadata", "--format-version", "1", "--locked", "--offline"];
/// The language every cataloged package's source is parsed as.
const RUST_LANGUAGE_NAME: &str = "rust";

/// The resolver for Rust packages, answering from `cargo metadata` or from `Cargo.lock`.
#[derive(Debug, Default)]
pub struct CargoResolver;

impl CargoResolver {
    /// The Cargo resolver. It holds no state, so one instance serves every workspace.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl DependencyResolver for CargoResolver {
    fn name(&self) -> ResolverName {
        ResolverName::Cargo
    }

    fn manager(&self) -> &'static str {
        CARGO_MANAGER
    }

    fn language(&self) -> Language {
        rust_language()
    }

    fn manifest_file_name(&self) -> &'static str {
        CARGO_MANIFEST_FILE_NAME
    }

    /// Catalogs the packages the request's manifests resolve to.
    ///
    /// Only the top-level manifests run: a manifest with another listed manifest in an
    /// ancestor directory is covered by that ancestor's answer, since `cargo metadata`
    /// lists every workspace member, and a nested manifest the ancestor does not list
    /// as a member is not resolved. Each top-level manifest runs `cargo metadata` with
    /// `--locked --offline`, so the resolver never writes a lockfile into the tree and
    /// never reaches the network; a run that fails degrades to the `Cargo.lock` beside
    /// the manifest. Every listed manifest and every top-level `Cargo.lock` is an input.
    /// A package two top-level manifests both resolve gets one entry per manifest;
    /// `DependencyCatalog::assemble` merges the two. The standard library is cataloged
    /// once per resolution from `rustc`. Entries stop at `PACKAGES_MAX` and the drop is
    /// reported. Selecting the top-level manifests compares every manifest pair, so that
    /// work is quadratic in the manifest count, which `MANIFESTS_MAX` bounds.
    fn resolve(
        &self,
        request: &ResolutionRequest<'_>,
        inspector: &mut dyn Inspector,
    ) -> Resolution {
        let mut answer = ResolutionBuilder::default();
        for manifest in request.manifests {
            answer.input(manifest.clone());
        }
        for manifest in top_level_manifests(request.manifests) {
            answer.input(file_beside(manifest, CARGO_LOCK_FILE_NAME));
            resolve_manifest(request.root, manifest, inspector, &mut answer);
        }
        toolchain::resolve_stdlib(request.root, inspector, &mut answer);
        answer.build()
    }
}

/// Catalogs one top-level manifest's packages from `cargo metadata`, else from `Cargo.lock`.
fn resolve_manifest(
    root: &Path,
    manifest: &ProjectPath,
    inspector: &mut dyn Inspector,
    answer: &mut ResolutionBuilder,
) {
    let directory = manifest_directory_path(root, manifest);
    let command = toolchain_command(CARGO_PROGRAM, &METADATA_ARGUMENTS, directory.clone());
    match run_metadata(&command, inspector) {
        Ok(metadata) => answer.entries(metadata_entries(&metadata)),
        Err(failure) => {
            let manifest_path = &manifest.0;
            answer.degradation(format!(
                "{manifest_path}: {failure}; answered from {CARGO_LOCK_FILE_NAME}"
            ));
            lockfile::resolve_lockfile(&directory, manifest, inspector, answer);
        }
    }
}

/// One toolchain invocation from its program, literal arguments, and directory.
fn toolchain_command(
    program: &'static str,
    arguments: &[&str],
    working_directory: PathBuf,
) -> ToolchainCommand {
    ToolchainCommand {
        program,
        arguments: arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
        working_directory,
    }
}

/// Why one toolchain run produced no usable answer: the command, and what went wrong.
#[derive(Debug)]
struct RunFailure {
    command: String,
    cause: RunFailureCause,
}

/// What stopped one toolchain run from answering.
#[derive(Debug)]
enum RunFailureCause {
    /// The inspector could not run the program; carries its reason.
    NotRun(String),
    /// The program exited nonzero; carries the first line of its standard error.
    Exited {
        exit_code: Option<i32>,
        stderr_line: String,
    },
    /// Standard output ran past `TOOLCHAIN_OUTPUT_BYTES_MAX` and was cut.
    OutputTruncated,
    /// Standard output was not the JSON document `cargo metadata` emits.
    UnparsableJson(serde_json::Error),
    /// `rustc --print sysroot` printed nothing.
    EmptySysroot,
    /// `rustc --version` printed a line without a version token; carries that line.
    MissingVersion(String),
}

impl fmt::Display for RunFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "`{}` {}", self.command, self.cause)
    }
}

impl fmt::Display for RunFailureCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRun(reason) => write!(formatter, "could not run: {reason}"),
            Self::Exited {
                exit_code,
                stderr_line,
            } => {
                match exit_code {
                    Some(code) => write!(formatter, "exited {code}")?,
                    None => formatter.write_str("exited without a code")?,
                }
                if stderr_line.is_empty() {
                    Ok(())
                } else {
                    write!(formatter, ": {stderr_line}")
                }
            }
            Self::OutputTruncated => write!(
                formatter,
                "wrote more than {TOOLCHAIN_OUTPUT_BYTES_MAX} bytes to standard output"
            ),
            Self::UnparsableJson(error) => {
                write!(formatter, "answered unparsable JSON: {error}")
            }
            Self::EmptySysroot => formatter.write_str("answered an empty sysroot"),
            Self::MissingVersion(line) => {
                write!(formatter, "answered `{line}` without a version token")
            }
        }
    }
}

/// One run failure naming the command it happened to.
fn run_failure(command: &ToolchainCommand, cause: RunFailureCause) -> RunFailure {
    RunFailure {
        command: command.rendered(),
        cause,
    }
}

/// Runs one command, answering its output only when the run succeeded whole.
fn run_to_success(
    command: &ToolchainCommand,
    inspector: &mut dyn Inspector,
) -> Result<CommandOutput, RunFailure> {
    let output = inspector
        .run(command)
        .map_err(|failure| run_failure(command, RunFailureCause::NotRun(failure.reason)))?;
    if output.succeeded() {
        return Ok(output);
    }
    if output.stdout_truncated {
        return Err(run_failure(command, RunFailureCause::OutputTruncated));
    }
    let cause = RunFailureCause::Exited {
        exit_code: output.exit_code,
        stderr_line: first_line(&output.stderr).to_owned(),
    };
    Err(run_failure(command, cause))
}

/// The first line of `text` without its ending; empty when `text` is.
fn first_line(text: &str) -> &str {
    lines_inclusive(text).next().map_or("", without_ending)
}

/// The `cargo metadata --format-version 1` document, the fields this resolver reads.
#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetadataPackage>,
    workspace_members: Vec<String>,
    resolve: Option<MetadataResolve>,
}

/// One resolved package: identity, id, and the manifest naming its source directory.
#[derive(Deserialize)]
struct MetadataPackage {
    name: String,
    version: String,
    id: String,
    manifest_path: PathBuf,
}

/// The resolved dependency graph.
#[derive(Deserialize)]
struct MetadataResolve {
    nodes: Vec<MetadataNode>,
}

/// One package's node in the resolved graph, with the ids it depends on.
#[derive(Deserialize)]
struct MetadataNode {
    id: String,
    deps: Vec<MetadataDependency>,
}

/// One edge of the resolved graph, naming the depended-on package id.
#[derive(Deserialize)]
struct MetadataDependency {
    pkg: String,
}

/// Runs `cargo metadata` and parses its answer.
fn run_metadata(
    command: &ToolchainCommand,
    inspector: &mut dyn Inspector,
) -> Result<Metadata, RunFailure> {
    let output = run_to_success(command, inspector)?;
    serde_json::from_str(&output.stdout)
        .map_err(|error| run_failure(command, RunFailureCause::UnparsableJson(error)))
}

/// One entry per package outside the workspace members, direct when a member depends on it.
///
/// A path dependency the workspace does not list as a member is a dependency like any
/// other; its manifest's directory is its source root.
fn metadata_entries(metadata: &Metadata) -> Vec<CatalogEntry> {
    let members: BTreeSet<&str> = metadata
        .workspace_members
        .iter()
        .map(String::as_str)
        .collect();
    let direct = member_dependencies(metadata, &members);
    metadata
        .packages
        .iter()
        .filter(|package| !members.contains(package.id.as_str()))
        .map(|package| {
            CatalogEntry::dependency(
                package_identity(CARGO_MANAGER, &package.name, &package.version),
                rust_language(),
                package.manifest_path.parent().map(Path::to_path_buf),
                direct.contains(package.id.as_str()),
            )
        })
        .collect()
}

/// The package ids every workspace member's resolve node depends on.
fn member_dependencies<'a>(metadata: &'a Metadata, members: &BTreeSet<&str>) -> BTreeSet<&'a str> {
    metadata
        .resolve
        .iter()
        .flat_map(|resolve| resolve.nodes.iter())
        .filter(|node| members.contains(node.id.as_str()))
        .flat_map(|node| node.deps.iter().map(|dependency| dependency.pkg.as_str()))
        .collect()
}

/// The Rust language, with no dialect.
fn rust_language() -> Language {
    Language {
        name: RUST_LANGUAGE_NAME.to_owned(),
        dialect: None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::fixture::{
        CARGO_HOME, METADATA_RENDERED, ROOT, entry, lockfile_inspector, metadata_runs, names,
        package, project, read_lockfile_count, registry_id, registry_manifest, resolve, with_rustc,
        workspace_metadata,
    };
    use super::*;
    use crate::catalog::PackageLocation;
    use crate::fixture::RecordedInspector;
    use crate::resolver::PACKAGES_MAX;
    use crate::resolvers::resolvers;

    #[test]
    fn test_cargo_resolver_identity_names_cargo_and_rust() {
        let resolver = CargoResolver::new();
        assert_eq!(resolver.name(), ResolverName::Cargo);
        assert_eq!(resolver.manager(), "cargo");
        assert_eq!(resolver.language().identity_segment(), "rust");
        assert_eq!(resolver.manifest_file_name(), "Cargo.toml");
    }

    #[test]
    fn test_resolve_metadata_success_catalogs_non_member_packages() {
        let mut inspector = with_rustc(RecordedInspector::default().with_command(
            METADATA_RENDERED,
            RecordedInspector::succeeded(workspace_metadata()),
        ));

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        assert_eq!(
            names(&resolution),
            [
                "cargo/local-tool@0.3.0",
                "cargo/serde@1.0.228",
                "cargo/serde_derive@1.0.228",
                "cargo/ty_project@0.0.0",
                "stdlib/rust@1.98.0",
            ]
        );
        let serde = entry(&resolution, "serde");
        assert_eq!(
            serde.source_root(),
            Some(Path::new(
                "/cargo-home/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.228"
            ))
        );
        assert!(serde.is_direct(), "a member depends on serde");
        assert_eq!(serde.location(), PackageLocation::Dependency);
        let serde_derive = entry(&resolution, "serde_derive");
        assert!(
            !serde_derive.is_direct(),
            "only serde depends on serde_derive"
        );
        assert!(serde_derive.source_root().is_some());
        let ty_project = entry(&resolution, "ty_project");
        assert!(ty_project.is_direct());
        assert_eq!(
            ty_project.source_root(),
            Some(Path::new(
                "/cargo-home/git/checkouts/ruff-b18f69e2b025fac7/2b0d210/crates/ty_project"
            ))
        );
        let local_tool = entry(&resolution, "local-tool");
        assert!(local_tool.is_direct());
        assert_eq!(
            local_tool.source_root(),
            Some(Path::new("/elsewhere/local-tool"))
        );
        assert_eq!(
            resolution.inputs,
            [project("Cargo.toml"), project("Cargo.lock")]
        );
        assert!(
            resolution.degradations.is_empty(),
            "{:?}",
            resolution.degradations
        );
        assert_eq!(
            metadata_runs(&inspector),
            [format!("run {METADATA_RENDERED} in {ROOT}")]
        );
        assert_eq!(
            read_lockfile_count(&inspector),
            0,
            "the toolchain answered; Cargo.lock stays unread"
        );
    }

    #[test]
    fn test_resolve_nested_manifest_runs_metadata_once_at_root() {
        let mut inspector = with_rustc(RecordedInspector::default().with_command(
            METADATA_RENDERED,
            RecordedInspector::succeeded(workspace_metadata()),
        ));

        let resolution = resolve(&["Cargo.toml", "crates/a/Cargo.toml"], &mut inspector);

        assert_eq!(
            metadata_runs(&inspector),
            [format!("run {METADATA_RENDERED} in {ROOT}")]
        );
        assert_eq!(
            resolution.inputs,
            [
                project("Cargo.toml"),
                project("crates/a/Cargo.toml"),
                project("Cargo.lock")
            ]
        );
    }

    #[test]
    fn test_resolve_two_top_level_manifests_runs_metadata_per_directory() {
        let mut inspector = with_rustc(RecordedInspector::default().with_command(
            METADATA_RENDERED,
            RecordedInspector::succeeded(workspace_metadata()),
        ));

        let resolution = resolve(
            &["crates/a/Cargo.toml", "tools/x/Cargo.toml"],
            &mut inspector,
        );

        assert_eq!(
            metadata_runs(&inspector),
            [
                format!("run {METADATA_RENDERED} in {ROOT}/crates/a"),
                format!("run {METADATA_RENDERED} in {ROOT}/tools/x"),
            ]
        );
        assert_eq!(
            resolution.inputs,
            [
                project("crates/a/Cargo.toml"),
                project("tools/x/Cargo.toml"),
                project("crates/a/Cargo.lock"),
                project("tools/x/Cargo.lock"),
            ]
        );
        assert_eq!(
            resolution.entries.len(),
            9,
            "one entry per manifest that resolved a package, plus the standard library; \
             assembly merges identities"
        );
    }

    #[test]
    fn test_resolve_command_unavailable_answers_from_lockfile() {
        let mut inspector = lockfile_inspector();

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations,
            [
                format!(
                    "Cargo.toml: `{METADATA_RENDERED}` could not run: failed to launch: No such \
                     file or directory (os error 2); answered from Cargo.lock"
                ),
                "rustc unavailable (`rustc --print sysroot` could not run: failed to launch: No \
                 such file or directory (os error 2)); no standard library entry"
                    .to_owned(),
            ]
        );
        assert_eq!(
            names(&resolution),
            [
                "cargo/itertools@0.15.0",
                "cargo/serde@1.0.228",
                "cargo/serde_derive@1.0.228",
                "cargo/ty_project@0.0.0",
            ]
        );
        let serde = entry(&resolution, "serde");
        assert_eq!(
            serde.source_root(),
            Some(Path::new(
                "/cargo-home/registry/src/index.crates.io-1949cf8c6b5b557f/serde-1.0.228"
            ))
        );
        assert!(serde.is_direct());
        let itertools = entry(&resolution, "itertools");
        assert!(
            itertools.is_direct(),
            "`name version` dependency spelling names the package"
        );
        assert!(itertools.source_root().is_some());
        let serde_derive = entry(&resolution, "serde_derive");
        assert_eq!(
            serde_derive.source_root(),
            None,
            "no cached directory, no root"
        );
        assert!(!serde_derive.is_direct());
        let ty_project = entry(&resolution, "ty_project");
        assert_eq!(
            ty_project.source_root(),
            Some(Path::new(
                "/cargo-home/git/checkouts/ruff-b18f69e2b025fac7/2b0d210"
            ))
        );
        assert!(ty_project.is_direct());
        assert_eq!(read_lockfile_count(&inspector), 1);
        assert!(
            inspector
                .asked
                .contains(&"environment CARGO_HOME".to_owned())
        );
        assert!(
            inspector
                .asked
                .contains(&format!("list {CARGO_HOME}/registry/src"))
        );
    }

    #[test]
    fn test_resolve_nonzero_exit_degrades_to_lockfile() {
        let mut inspector = lockfile_inspector().with_command(
            METADATA_RENDERED,
            RecordedInspector::failed("error: the lock file needs to be updated\nsecond line\n"),
        );

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations[0],
            format!(
                "Cargo.toml: `{METADATA_RENDERED}` exited 101: error: the lock file needs to be \
                 updated; answered from Cargo.lock"
            )
        );
        assert_eq!(resolution.entries.len(), 4);
        assert_eq!(read_lockfile_count(&inspector), 1);
    }

    #[test]
    fn test_resolve_truncated_stdout_degrades_to_lockfile() {
        let truncated = Ok(CommandOutput {
            exit_code: Some(0),
            stdout: "{\"packages\": [".to_owned(),
            stderr: String::new(),
            stdout_truncated: true,
        });
        let mut inspector = lockfile_inspector().with_command(METADATA_RENDERED, truncated);

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations[0],
            format!(
                "Cargo.toml: `{METADATA_RENDERED}` wrote more than {TOOLCHAIN_OUTPUT_BYTES_MAX} \
                 bytes to standard output; answered from Cargo.lock"
            )
        );
        assert_eq!(resolution.entries.len(), 4);
    }

    #[test]
    fn test_resolve_unparsable_metadata_degrades_to_lockfile() {
        let mut inspector = lockfile_inspector()
            .with_command(METADATA_RENDERED, RecordedInspector::succeeded("not json"));

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        let degradation = &resolution.degradations[0];
        assert!(
            degradation.starts_with(&format!(
                "Cargo.toml: `{METADATA_RENDERED}` answered unparsable JSON: "
            )),
            "{degradation}"
        );
        assert!(
            degradation.ends_with("; answered from Cargo.lock"),
            "{degradation}"
        );
        assert_eq!(resolution.entries.len(), 4);
    }

    #[test]
    fn test_resolve_exit_without_code_or_stderr_names_the_exit() {
        let signaled = Ok(CommandOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
        });
        let mut inspector = lockfile_inspector().with_command(METADATA_RENDERED, signaled);

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        assert_eq!(
            resolution.degradations[0],
            format!(
                "Cargo.toml: `{METADATA_RENDERED}` exited without a code; answered from Cargo.lock"
            )
        );
    }

    #[test]
    fn test_resolve_packages_over_max_drops_excess_with_degradation() {
        let member = format!("path+file://{ROOT}#0.1.0");
        let mut packages = vec![package(
            "app",
            "0.1.0",
            &member,
            &format!("{ROOT}/Cargo.toml"),
        )];
        for index in 0..=PACKAGES_MAX {
            let name = format!("pkg-{index:05}");
            let id = registry_id(&name, "1.0.0");
            packages.push(package(
                &name,
                "1.0.0",
                &id,
                &registry_manifest(&name, "1.0.0"),
            ));
        }
        let metadata = json!({
            "packages": packages,
            "workspace_members": [member],
            "resolve": null,
        })
        .to_string();
        let mut inspector = RecordedInspector::default()
            .with_command(METADATA_RENDERED, RecordedInspector::succeeded(metadata));

        let resolution = resolve(&["Cargo.toml"], &mut inspector);

        assert_eq!(resolution.entries.len(), PACKAGES_MAX);
        assert!(resolution.entries.iter().all(|entry| !entry.is_direct()));
        assert_eq!(
            resolution.degradations,
            [
                "rustc unavailable (`rustc --print sysroot` could not run: failed to launch: No \
                 such file or directory (os error 2)); no standard library entry"
                    .to_owned(),
                format!(
                    "1 of {} packages were not cataloged: at most {PACKAGES_MAX} are cataloged \
                     per workspace",
                    PACKAGES_MAX + 1
                ),
            ]
        );
    }

    #[test]
    fn test_resolve_same_inspector_twice_answers_equal_resolutions() {
        let first = resolve(&["Cargo.toml"], &mut with_rustc(lockfile_inspector()));
        let second = resolve(&["Cargo.toml"], &mut with_rustc(lockfile_inspector()));

        assert_eq!(first, second);
        assert_eq!(first.entries.len(), 5);
    }
}
