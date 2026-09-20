//! The server's filesystem-backed inspector, and workspace dependency resolution through it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rift_dependency::{
    CommandFailure, CommandOutput, DependencyCatalog, DependencyContext, FileObservation,
    Inspector, StaticInputs, TOOLCHAIN_OUTPUT_BYTES_MAX, ToolchainCommand,
};
use rift_protocol::dependencies::{
    ConfiguredPackage, DependenciesConfiguration, DependencyResolution,
};
use rift_protocol::read::ProjectPath;

use crate::process::{BoundedRun, run_bounded};

/// The reason every toolchain run is refused under `resolution = "static"`.
const STATIC_RESOLUTION_REASON: &str = "resolution = static: toolchains are not run";

/// How the inspector answers a toolchain run: whether one runs at all, and how long it
/// may take. Read from the `[dependencies]` table's `resolution` and `command_timeout`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResolutionPolicy {
    /// Whether a toolchain runs. `false` refuses every run before anything spawns.
    execution: bool,
    /// Wall clock one run may take before the inspector kills it.
    command_timeout: Duration,
}

impl From<&DependenciesConfiguration> for ResolutionPolicy {
    fn from(configuration: &DependenciesConfiguration) -> Self {
        Self {
            execution: configuration.resolution == DependencyResolution::Auto,
            command_timeout: Duration::from_millis(configuration.command_timeout.milliseconds()),
        }
    }
}

impl Default for ResolutionPolicy {
    /// The default `[dependencies]` table's policy.
    fn default() -> Self {
        Self::from(&DependenciesConfiguration::default())
    }
}

/// The inspector that answers from this machine: its files, environment, and `PATH`.
#[derive(Debug)]
pub(crate) struct FilesystemInspector {
    policy: ResolutionPolicy,
}

impl FilesystemInspector {
    /// An inspector running toolchains under `policy`.
    pub(crate) const fn new(policy: ResolutionPolicy) -> Self {
        Self { policy }
    }
}

impl StaticInputs for FilesystemInspector {
    /// The regular file at `path` when it fits `bytes_max`. A larger file
    /// answers its size, and anything else answers absent.
    fn read_file(&mut self, path: &Path, bytes_max: u64) -> FileObservation {
        match fs::metadata(path) {
            Ok(metadata) if !metadata.is_file() => FileObservation::Absent,
            Ok(metadata) if metadata.len() > bytes_max => FileObservation::OverBound {
                bytes: metadata.len(),
            },
            Ok(_) => fs::read(path).map_or(FileObservation::Absent, FileObservation::Bytes),
            Err(_) => FileObservation::Absent,
        }
    }
}

impl Inspector for FilesystemInspector {
    fn directory_exists(&mut self, path: &Path) -> bool {
        fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
    }

    /// The UTF-8 entry names below `path`, in name order, at most `entries_max`.
    /// The cut is the trait's contract: a directory past the bound answers its
    /// first `entries_max` names and nothing marks the rest. An absent or
    /// unreadable directory answers empty.
    fn list_directory(&mut self, path: &Path, entries_max: usize) -> Vec<String> {
        let Ok(entries) = fs::read_dir(path) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort_unstable();
        names.truncate(entries_max);
        names
    }

    /// Runs the program from `PATH` under the policy's bounds.
    ///
    /// The run is cut at the policy's `command_timeout` and its standard output
    /// at [`TOOLCHAIN_OUTPUT_BYTES_MAX`]. A program spelled as a path, and every
    /// program while the policy runs no toolchain, is refused before anything
    /// spawns.
    fn run(&mut self, command: &ToolchainCommand) -> Result<CommandOutput, CommandFailure> {
        let program = command.program;
        if !self.policy.execution {
            return Err(failure(program, STATIC_RESOLUTION_REASON));
        }
        if !is_bare_program(program) {
            return Err(failure(
                program,
                "program must be a bare name resolved on PATH",
            ));
        }
        let mut process = Command::new(program);
        process
            .args(&command.arguments)
            .current_dir(&command.working_directory);
        let run = run_bounded(
            &mut process,
            self.policy.command_timeout,
            toolchain_capture_bytes(),
        )
        .map_err(|io| failure(program, format!("failed to launch: {io}")))?;
        output_of(program, run, self.policy.command_timeout)
    }

    fn environment(&mut self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn home_directory(&mut self) -> Option<PathBuf> {
        std::env::home_dir()
    }
}

/// Whether `program` is a bare name for `PATH` to resolve, not a path of its own.
fn is_bare_program(program: &str) -> bool {
    let absolute = Path::new(program).is_absolute();
    let has_separator = program.contains(std::path::is_separator);
    !absolute && !has_separator
}

/// [`TOOLCHAIN_OUTPUT_BYTES_MAX`] as a capture size. A target too narrow to
/// hold it captures up to the drain ceiling instead.
fn toolchain_capture_bytes() -> usize {
    usize::try_from(TOOLCHAIN_OUTPUT_BYTES_MAX).unwrap_or(usize::MAX)
}

/// The run's output, or the failure that left it without one. `timeout` is the
/// bound a killed run overstayed, named in its failure.
fn output_of(
    program: &str,
    run: BoundedRun,
    timeout: Duration,
) -> Result<CommandOutput, CommandFailure> {
    let exit = match run.exit {
        _ if run.timed_out => {
            let seconds = timeout.as_secs();
            return Err(failure(
                program,
                format!("overstayed {seconds}s and was killed"),
            ));
        }
        Err(io) => return Err(failure(program, format!("waiting on the process: {io}"))),
        Ok(exit) => exit,
    };
    Ok(CommandOutput {
        exit_code: exit.code(),
        stdout: run.stdout.text,
        stderr: run.stderr.text,
        stdout_truncated: run.stdout.truncated,
    })
}

fn failure(program: &str, reason: impl Into<String>) -> CommandFailure {
    CommandFailure {
        program: program.to_owned(),
        reason: reason.into(),
    }
}

/// Resolves the dependency catalog of one workspace through the shipped resolvers.
///
/// Every file read and toolchain run goes through a [`FilesystemInspector`] under
/// `policy`. The span records the entry count and whether the answer degraded, and
/// each degradation is logged once as a warning.
pub(crate) fn resolve_workspace_catalog(
    root: &Path,
    visible: &[ProjectPath],
    policy: ResolutionPolicy,
) -> DependencyCatalog {
    let span = tracing::info_span!(
        "dependency.resolve",
        component = "dependency",
        entries = tracing::field::Empty,
        degraded = tracing::field::Empty,
    );
    let _entered = span.enter();
    let catalog = rift_dependency::resolve_catalog(
        root,
        visible,
        rift_dependency::resolvers(),
        &mut FilesystemInspector::new(policy),
    );
    span.record("entries", catalog.entries().len());
    span.record("degraded", catalog.is_degraded());
    for degradation in catalog.degradations() {
        tracing::warn!(
            component = "dependency",
            resolver = %degradation.resolver,
            reason = %degradation.reason,
            "dependency resolution degraded"
        );
    }
    catalog
}

/// Reads the static dependency context of one workspace through the shipped resolvers.
///
/// The pass reads manifests and lockfiles alone: the inspector reaches it as a
/// [`StaticInputs`], which offers a file read and nothing else, so no toolchain runs and
/// no package cache is inspected whatever the `[dependencies]` table says. The span
/// records the entry count and whether an input went unread, and each degradation is
/// logged once as a warning.
pub(crate) fn read_workspace_context(
    root: &Path,
    visible: &[ProjectPath],
    configured: &[ConfiguredPackage],
) -> DependencyContext {
    let span = tracing::info_span!(
        "dependency.context",
        component = "dependency",
        entries = tracing::field::Empty,
        degraded = tracing::field::Empty,
    );
    let _entered = span.enter();
    let context = rift_dependency::resolve_context(
        root,
        visible,
        rift_dependency::resolvers(),
        &mut FilesystemInspector::new(ResolutionPolicy::default()),
        configured,
    );
    span.record("entries", context.entries().len());
    span.record("degraded", context.is_degraded());
    for degradation in context.degradations() {
        tracing::warn!(
            component = "dependency",
            resolver = %degradation.resolver,
            reason = %degradation.reason,
            "dependency context degraded"
        );
    }
    context
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn command(program: &'static str, arguments: &[&str], directory: &Path) -> ToolchainCommand {
        ToolchainCommand {
            program,
            arguments: arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
            working_directory: directory.to_path_buf(),
        }
    }

    fn inspector() -> FilesystemInspector {
        FilesystemInspector::new(ResolutionPolicy::default())
    }

    /// The policy `resolution = "static"` compiles to.
    fn static_policy() -> ResolutionPolicy {
        ResolutionPolicy::from(&DependenciesConfiguration {
            resolution: DependencyResolution::Static,
            ..DependenciesConfiguration::default()
        })
    }

    /// One project whose lockfile pins a registry package, laid out below `root`.
    fn write_locked_project(root: &Path) -> Vec<ProjectPath> {
        std::fs::create_dir(root.join("src")).expect("create src");
        std::fs::write(root.join("src/lib.rs"), "").expect("write lib.rs");
        let manifest = "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                        [dependencies]\nserde = \"1\"\n";
        std::fs::write(root.join("Cargo.toml"), manifest).expect("write manifest");
        let lockfile = "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n\
                        dependencies = [\n \"serde\",\n]\n\n[[package]]\nname = \"serde\"\n\
                        version = \"1.0.228\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n\
                        checksum = \"9a8e94ea7f378bd32cbbd37198a4a91436180c5bb472411e48b5ec2e2124ae9e\"\n";
        std::fs::write(root.join("Cargo.lock"), lockfile).expect("write lockfile");
        vec![
            ProjectPath("Cargo.lock".to_owned()),
            ProjectPath("Cargo.toml".to_owned()),
            ProjectPath("src/lib.rs".to_owned()),
        ]
    }

    #[test]
    fn test_policy_reads_resolution_and_command_timeout_from_the_table() {
        let table = DependenciesConfiguration {
            command_timeout: rift_protocol::configuration::Duration::from_millis(5_000),
            ..DependenciesConfiguration::default()
        };
        let policy = ResolutionPolicy::from(&table);
        assert!(policy.execution);
        assert_eq!(policy.command_timeout, Duration::from_secs(5));
        assert!(!static_policy().execution);
        assert_eq!(
            ResolutionPolicy::default(),
            ResolutionPolicy::from(&DependenciesConfiguration::default())
        );
    }

    #[test]
    fn test_read_file_answers_absent_bytes_and_over_bound() {
        let directory = tempfile::tempdir().expect("tempdir");
        let manifest = directory.path().join("Cargo.toml");
        std::fs::write(&manifest, b"[package]\n").expect("write manifest");
        let mut inspector = inspector();
        let missing = inspector.read_file(&directory.path().join("missing"), 64);
        assert_eq!(missing, FileObservation::Absent);
        let not_a_file = inspector.read_file(directory.path(), 64);
        assert_eq!(
            not_a_file,
            FileObservation::Absent,
            "a directory is not a file"
        );
        let within = inspector.read_file(&manifest, 64);
        assert_eq!(within, FileObservation::Bytes(b"[package]\n".to_vec()));
        let exact = inspector.read_file(&manifest, 10);
        assert_eq!(exact, FileObservation::Bytes(b"[package]\n".to_vec()));
        let over = inspector.read_file(&manifest, 9);
        assert_eq!(over, FileObservation::OverBound { bytes: 10 });
    }

    #[test]
    fn test_directory_exists_tells_a_directory_from_a_file_and_nothing() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = directory.path().join("file");
        std::fs::write(&file, b"").expect("write file");
        let mut inspector = inspector();
        assert!(inspector.directory_exists(directory.path()));
        assert!(!inspector.directory_exists(&file));
        assert!(!inspector.directory_exists(&directory.path().join("missing")));
    }

    #[test]
    fn test_list_directory_sorts_names_and_stops_at_entries_max() {
        let directory = tempfile::tempdir().expect("tempdir");
        for name in ["c", "a", "b"] {
            std::fs::write(directory.path().join(name), b"").expect("write entry");
        }
        let mut inspector = inspector();
        assert_eq!(
            inspector.list_directory(directory.path(), 10),
            ["a", "b", "c"]
        );
        assert_eq!(
            inspector.list_directory(directory.path(), 3),
            ["a", "b", "c"]
        );
        assert_eq!(inspector.list_directory(directory.path(), 2), ["a", "b"]);
        assert!(inspector.list_directory(directory.path(), 0).is_empty());
        let missing = inspector.list_directory(&directory.path().join("missing"), 10);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_run_captures_stdout_and_a_zero_exit() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut inspector = inspector();
        let probe = command("sh", &["-c", "printf ok"], directory.path());
        let output = inspector.run(&probe).expect("sh runs");
        let expected = CommandOutput {
            exit_code: Some(0),
            stdout: "ok".to_owned(),
            stderr: String::new(),
            stdout_truncated: false,
        };
        assert_eq!(output, expected);
        assert!(output.succeeded());
    }

    #[test]
    fn test_run_reports_a_nonzero_exit_code_and_stderr() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut inspector = inspector();
        let probe = command("sh", &["-c", "printf err >&2; exit 3"], directory.path());
        let output = inspector.run(&probe).expect("sh runs");
        assert_eq!(output.exit_code, Some(3));
        assert_eq!(output.stderr, "err");
        assert!(!output.succeeded());
    }

    #[test]
    fn test_run_starts_in_the_working_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut inspector = inspector();
        let output = inspector
            .run(&command("pwd", &[], directory.path()))
            .expect("pwd runs");
        let printed = std::fs::canonicalize(output.stdout.trim_end()).expect("printed resolves");
        let expected = std::fs::canonicalize(directory.path()).expect("tempdir resolves");
        assert_eq!(printed, expected);
    }

    #[test]
    fn test_run_reports_a_missing_program_as_a_launch_failure() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut inspector = inspector();
        let probe = command(
            "rift-test-binary-that-does-not-exist",
            &[],
            directory.path(),
        );
        let failure = inspector
            .run(&probe)
            .expect_err("a missing program cannot launch");
        assert_eq!(failure.program, "rift-test-binary-that-does-not-exist");
        assert!(failure.reason.starts_with("failed to launch"), "{failure}");
    }

    #[test]
    fn test_run_refuses_a_program_spelled_as_a_path_before_spawning() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut inspector = inspector();
        for program in ["/bin/sh", "bin/sh"] {
            let failure = inspector
                .run(&command(program, &["-c", "printf ran"], directory.path()))
                .expect_err("a path is not a bare program name");
            assert_eq!(failure.program, program);
            assert_eq!(
                failure.reason,
                "program must be a bare name resolved on PATH"
            );
        }
    }

    /// Under `resolution = "static"` no program runs, bare or not: the refusal names
    /// the policy and nothing spawns.
    #[test]
    fn test_run_refuses_every_program_under_static_resolution_before_spawning() {
        let directory = tempfile::tempdir().expect("tempdir");
        let marker = directory.path().join("ran");
        let touch = format!("touch {}", marker.display());
        let mut inspector = FilesystemInspector::new(static_policy());
        let failure = inspector
            .run(&command("sh", &["-c", &touch], directory.path()))
            .expect_err("a static policy runs nothing");
        assert_eq!(failure.program, "sh");
        assert_eq!(failure.reason, STATIC_RESOLUTION_REASON);
        assert!(!marker.exists(), "the program must not have run");
    }

    #[test]
    fn test_run_inherits_the_server_environment() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut inspector = inspector();
        let probe = command("sh", &["-c", r#"printf "%s" "$PATH""#], directory.path());
        let output = inspector.run(&probe).expect("sh runs");
        let expected = std::env::var("PATH").expect("the test process has a PATH");
        assert_eq!(output.stdout, expected);
    }

    #[test]
    fn test_environment_and_home_directory_answer_from_the_process() {
        let mut inspector = inspector();
        assert_eq!(inspector.environment("PATH"), std::env::var("PATH").ok());
        assert_eq!(inspector.environment("RIFT_DEPENDENCY_PROBE_UNSET"), None);
        assert_eq!(inspector.home_directory(), std::env::home_dir());
    }

    #[test]
    fn test_resolve_workspace_catalog_reads_the_manifest_and_lockfile() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        std::fs::create_dir(root.join("src")).expect("create src");
        std::fs::write(root.join("src/lib.rs"), "").expect("write lib.rs");
        let manifest = "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
        std::fs::write(root.join("Cargo.toml"), manifest).expect("write manifest");
        let lockfile = "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n";
        std::fs::write(root.join("Cargo.lock"), lockfile).expect("write lockfile");
        let visible = [
            ProjectPath("Cargo.lock".to_owned()),
            ProjectPath("Cargo.toml".to_owned()),
            ProjectPath("src/lib.rs".to_owned()),
        ];

        let catalog = resolve_workspace_catalog(root, &visible, ResolutionPolicy::default());

        let inputs: Vec<&str> = catalog.inputs().map(|path| path.0.as_str()).collect();
        assert_eq!(
            inputs,
            ["Cargo.lock", "Cargo.toml"],
            "{:?}",
            catalog.degradations()
        );
        assert!(!catalog.is_degraded(), "{:?}", catalog.degradations());
    }

    /// Under `resolution = "static"` the Cargo resolver answers from `Cargo.lock` and
    /// the standard library entry, which only `rustc` can name, is a degradation.
    #[test]
    fn test_resolve_workspace_catalog_static_reads_the_lockfile_and_degrades_the_toolchain() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        let visible = write_locked_project(root);

        let catalog = resolve_workspace_catalog(root, &visible, static_policy());

        let inputs: Vec<&str> = catalog.inputs().map(|path| path.0.as_str()).collect();
        assert_eq!(inputs, ["Cargo.lock", "Cargo.toml"]);
        let names: Vec<String> = catalog
            .entries()
            .iter()
            .map(|entry| format!("{}/{}", entry.identity().manager, entry.identity().name))
            .collect();
        assert_eq!(
            names,
            ["cargo/serde"],
            "the lockfile's package is cataloged"
        );
        assert!(catalog.is_degraded());
        let reasons: Vec<&str> = catalog
            .degradations()
            .iter()
            .map(|degradation| degradation.reason.as_str())
            .collect();
        assert_eq!(reasons.len(), 2, "{reasons:?}");
        assert!(
            reasons[0].starts_with("Cargo.toml: ")
                && reasons[0].contains(STATIC_RESOLUTION_REASON)
                && reasons[0].ends_with("; answered from Cargo.lock"),
            "{}",
            reasons[0]
        );
        assert!(
            reasons[1].starts_with("rustc unavailable (")
                && reasons[1].contains(STATIC_RESOLUTION_REASON)
                && reasons[1].ends_with("; no standard library entry"),
            "{}",
            reasons[1]
        );
    }

    #[test]
    fn test_resolve_workspace_catalog_without_manifests_is_empty() {
        let directory = tempfile::tempdir().expect("tempdir");
        let visible = [ProjectPath("src/lib.rs".to_owned())];
        let catalog =
            resolve_workspace_catalog(directory.path(), &visible, ResolutionPolicy::default());
        assert!(catalog.entries().is_empty());
        assert_eq!(catalog.inputs().count(), 0);
        assert!(!catalog.is_degraded());
    }

    #[test]
    fn test_output_of_reports_a_killed_run_as_overstayed() {
        use std::os::unix::process::ExitStatusExt as _;
        let run = BoundedRun {
            exit: Ok(std::process::ExitStatus::from_raw(9)),
            timed_out: true,
            stdout: rift_core::CapturedStream::default(),
            stderr: rift_core::CapturedStream::default(),
        };

        let failure = output_of("cargo", run, Duration::from_secs(7))
            .expect_err("a killed run has no output");

        assert_eq!(failure.program, "cargo");
        assert_eq!(failure.reason, "overstayed 7s and was killed");
    }

    #[test]
    fn test_output_of_reports_an_unobservable_run_with_the_io_text() {
        let run = BoundedRun {
            exit: Err(std::io::Error::other("lost the child")),
            timed_out: false,
            stdout: rift_core::CapturedStream::default(),
            stderr: rift_core::CapturedStream::default(),
        };

        let failure = output_of("cargo", run, Duration::from_secs(7))
            .expect_err("an unobserved run has no output");

        assert_eq!(failure.reason, "waiting on the process: lost the child");
    }
}
