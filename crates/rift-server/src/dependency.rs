//! The server's filesystem-backed context inputs, and the dependency context read through them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rift_dependency::{
    CommandFailure, CommandOutput, ContextInputs, DependencyContext, FileObservation,
    StandardLibrary, StandardLibraryRequest, StaticInputs, TOOLCHAIN_OUTPUT_BYTES_MAX,
    ToolchainCommand,
};
use rift_protocol::dependencies::{
    ConfiguredPackage, DependenciesConfiguration, DependencyResolution,
};
use rift_protocol::read::ProjectPath;

use crate::process::{BoundedRun, run_bounded};

/// The reason every probe is refused under `resolution = "static"`.
const STATIC_RESOLUTION_REASON: &str = "resolution = static: toolchains are not run";

/// How the inputs answer a version probe: whether one runs at all, and how long it may
/// take. Read from the `[dependencies]` table's `resolution` and `command_timeout`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResolutionPolicy {
    /// Whether a probe runs. `false` refuses every run before anything spawns.
    execution: bool,
    /// Wall clock one run may take before the inputs kill it.
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

/// The context inputs that answer from this machine: its files and its `PATH`.
#[derive(Debug)]
pub(crate) struct FilesystemInputs {
    policy: ResolutionPolicy,
}

impl FilesystemInputs {
    /// Inputs running the version probes under `policy`.
    pub(crate) const fn new(policy: ResolutionPolicy) -> Self {
        Self { policy }
    }
}

impl StaticInputs for FilesystemInputs {
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

    fn canonical_path(&mut self, path: &Path) -> Option<PathBuf> {
        fs::canonicalize(path).ok()
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
}

impl ContextInputs for FilesystemInputs {
    /// Runs the program from `PATH` under the policy's bounds.
    ///
    /// The run is cut at the policy's `command_timeout` and its standard output
    /// at [`TOOLCHAIN_OUTPUT_BYTES_MAX`]; the command's environment overlay wins over
    /// the inherited value. A program spelled as a path, and every
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
            .current_dir(&command.working_directory)
            .envs(command.environment.iter().copied());
        for name in &command.environment_removed {
            process.env_remove(name);
        }
        let run = run_bounded(
            &mut process,
            self.policy.command_timeout,
            toolchain_capture_bytes(),
        )
        .map_err(|io| failure(program, format!("failed to launch: {io}")))?;
        output_of(program, run, self.policy.command_timeout)
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

/// Reads the dependency context of one workspace through the shipped resolvers, then
/// adds the standard library entries `libraries` names.
///
/// The resolvers read manifests and lockfiles alone: they reach the inputs as a
/// [`StaticInputs`], which offers a file read and a directory listing and nothing else.
/// The one program the pass runs is a standard library version probe, under `policy`'s
/// `command_timeout` and only while `policy` allows execution. The span records the entry count and
/// whether an input went unread, and each degradation is logged once as a warning.
pub(crate) fn read_workspace_context(
    root: &Path,
    visible: &[ProjectPath],
    configured: &[ConfiguredPackage],
    policy: ResolutionPolicy,
    libraries: &[StandardLibrary],
) -> DependencyContext {
    let span = rift_tracing::info_span!(
        "dependency.context",
        component = "dependency",
        entries = rift_tracing::empty!(),
        degraded = rift_tracing::empty!(),
    );
    span.in_scope(|| -> DependencyContext {
        let mut inputs = FilesystemInputs::new(policy);
        let mut context = rift_dependency::resolve_context(
            root,
            visible,
            rift_dependency::resolvers(),
            &mut inputs,
            configured,
        );
        let request = StandardLibraryRequest {
            root,
            libraries,
            execution: policy.execution,
        };
        context.add_standard_libraries(rift_dependency::standard_library_answer(
            &request,
            &mut inputs,
        ));
        span.record("entries", context.entries().len());
        span.record("degraded", context.is_degraded());
        for degradation in context.degradations() {
            rift_tracing::warn!(
                component = "dependency",
                resolver = %degradation.resolver,
                reason = %degradation.reason,
                "dependency context degraded"
            );
        }
        context
    })
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
            environment: Vec::new(),
            environment_removed: Vec::new(),
        }
    }

    fn inputs() -> FilesystemInputs {
        FilesystemInputs::new(ResolutionPolicy::default())
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
        let mut inputs = inputs();
        let missing = inputs.read_file(&directory.path().join("missing"), 64);
        assert_eq!(missing, FileObservation::Absent);
        let not_a_file = inputs.read_file(directory.path(), 64);
        assert_eq!(
            not_a_file,
            FileObservation::Absent,
            "a directory is not a file"
        );
        let within = inputs.read_file(&manifest, 64);
        assert_eq!(within, FileObservation::Bytes(b"[package]\n".to_vec()));
        let exact = inputs.read_file(&manifest, 10);
        assert_eq!(exact, FileObservation::Bytes(b"[package]\n".to_vec()));
        let over = inputs.read_file(&manifest, 9);
        assert_eq!(over, FileObservation::OverBound { bytes: 10 });
    }

    #[test]
    fn test_list_directory_sorts_names_and_stops_at_entries_max() {
        let directory = tempfile::tempdir().expect("tempdir");
        for name in ["c", "a", "b"] {
            std::fs::write(directory.path().join(name), b"").expect("write entry");
        }
        let mut inputs = inputs();
        assert_eq!(inputs.list_directory(directory.path(), 10), ["a", "b", "c"]);
        assert_eq!(inputs.list_directory(directory.path(), 3), ["a", "b", "c"]);
        assert_eq!(inputs.list_directory(directory.path(), 2), ["a", "b"]);
        assert!(inputs.list_directory(directory.path(), 0).is_empty());
        let missing = inputs.list_directory(&directory.path().join("missing"), 10);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_run_captures_stdout_and_a_zero_exit() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut inputs = inputs();
        let probe = command("sh", &["-c", "printf ok"], directory.path());
        let output = inputs.run(&probe).expect("sh runs");
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
        let mut inputs = inputs();
        let probe = command("sh", &["-c", "printf err >&2; exit 3"], directory.path());
        let output = inputs.run(&probe).expect("sh runs");
        assert_eq!(output.exit_code, Some(3));
        assert_eq!(output.stderr, "err");
        assert!(!output.succeeded());
    }

    #[test]
    fn test_run_starts_in_the_working_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut inputs = inputs();
        let output = inputs
            .run(&command("pwd", &[], directory.path()))
            .expect("pwd runs");
        let printed = std::fs::canonicalize(output.stdout.trim_end()).expect("printed resolves");
        let expected = std::fs::canonicalize(directory.path()).expect("tempdir resolves");
        assert_eq!(printed, expected);
    }

    #[test]
    fn test_run_reports_a_missing_program_as_a_launch_failure() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut inputs = inputs();
        let probe = command(
            "rift-test-binary-that-does-not-exist",
            &[],
            directory.path(),
        );
        let failure = inputs
            .run(&probe)
            .expect_err("a missing program cannot launch");
        assert_eq!(failure.program, "rift-test-binary-that-does-not-exist");
        assert!(failure.reason.starts_with("failed to launch"), "{failure}");
    }

    #[test]
    fn test_run_refuses_a_program_spelled_as_a_path_before_spawning() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut inputs = inputs();
        for program in ["/bin/sh", "bin/sh"] {
            let failure = inputs
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
        let mut inputs = FilesystemInputs::new(static_policy());
        let failure = inputs
            .run(&command("sh", &["-c", &touch], directory.path()))
            .expect_err("a static policy runs nothing");
        assert_eq!(failure.program, "sh");
        assert_eq!(failure.reason, STATIC_RESOLUTION_REASON);
        assert!(!marker.exists(), "the program must not have run");
    }

    #[test]
    fn test_run_inherits_the_server_environment() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut inputs = inputs();
        let probe = command("sh", &["-c", r#"printf "%s" "$PATH""#], directory.path());
        let output = inputs.run(&probe).expect("sh runs");
        let expected = std::env::var("PATH").expect("the test process has a PATH");
        assert_eq!(output.stdout, expected);
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

    #[test]
    fn test_run_lays_the_command_overlay_over_the_inherited_environment() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut overlaid = command("printenv", &["RUSTUP_AUTO_INSTALL"], directory.path());
        overlaid.environment = vec![("RUSTUP_AUTO_INSTALL", "0")];
        let output = inputs().run(&overlaid).expect("printenv runs");
        assert_eq!(output.stdout, "0\n");
    }

    #[test]
    fn test_run_removes_the_named_variables_from_the_inherited_environment() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut removed = command("printenv", &["HOME"], directory.path());
        removed.environment_removed = vec!["HOME"];
        let output = inputs().run(&removed).expect("printenv runs");
        assert_eq!(output.stdout, "");
        assert_ne!(
            output.exit_code,
            Some(0),
            "printenv exits nonzero on an unset name"
        );
    }

    /// Every file below `root` with its bytes, in path order.
    fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory).expect("read_dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let bytes = std::fs::read(&path).expect("read");
                    files.push((path, bytes));
                }
            }
        }
        files.sort();
        files
    }

    /// The installed toolchain names under `RUSTUP_HOME`, or `~/.rustup`.
    fn installed_toolchains() -> Vec<String> {
        let home = std::env::var_os("RUSTUP_HOME").map_or_else(
            || std::env::home_dir().unwrap_or_default().join(".rustup"),
            PathBuf::from,
        );
        let mut names: Vec<String> = std::fs::read_dir(home.join("toolchains"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn test_context_read_under_an_absent_pinned_toolchain_changes_no_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        let mut visible = write_locked_project(root);
        std::fs::write(
            root.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.80.0\"\n",
        )
        .expect("write toolchain file");
        std::fs::write(root.join(".nvmrc"), "22.3.0\n").expect("write nvmrc");
        std::fs::write(root.join("tool.py"), "").expect("write tool.py");
        visible.push(ProjectPath("tool.py".to_owned()));
        let files_before = snapshot(root);
        let toolchains_before = installed_toolchains();

        let context = read_workspace_context(
            root,
            &visible,
            &[],
            ResolutionPolicy::default(),
            &[
                StandardLibrary::Rust,
                StandardLibrary::Node,
                StandardLibrary::Python,
            ],
        );

        assert_eq!(snapshot(root), files_before);
        assert_eq!(installed_toolchains(), toolchains_before);
        let named: Vec<String> = context
            .entries()
            .iter()
            .map(|entry| format!("{}/{}", entry.manager, entry.name))
            .collect();
        assert_eq!(
            named,
            [
                "cargo/serde",
                "npm/typescript",
                "stdlib/node",
                "stdlib/python",
                "stdlib/rust"
            ]
        );
        let selector = |name: &str| {
            context
                .entries()
                .iter()
                .find(|entry| entry.manager == "stdlib" && entry.name == name)
                .map(|entry| (entry.version.clone(), entry.requirement.clone()))
        };
        assert_eq!(
            selector("rust"),
            Some((Some("1.80.0".to_owned()), None)),
            "the pin goes out whether or not rustc could run it: {:?}",
            context.degradations()
        );
        assert_eq!(selector("node"), Some((Some("22.3.0".to_owned()), None)));
        assert_eq!(selector("python"), Some((None, Some(">=0".to_owned()))));
    }
}
