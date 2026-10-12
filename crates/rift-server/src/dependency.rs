//! The server's filesystem-backed context inputs, and the dependency context read through them.

use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(test)]
use rift_dependency::TOOLCHAIN_OUTPUT_BYTES_MAX;
use rift_dependency::{
    CommandFailure, CommandOutput, ContextInputs, DependencyContext, FileObservation,
    StandardLibrary, StandardLibraryRequest, StaticInputs, ToolchainCommand,
};
use rift_protocol::dependencies::{
    ConfiguredPackage, DependenciesConfiguration, DependencyResolution,
};
use rift_protocol::read::ProjectPath;

use rift_error::RiftError;
use rift_error::errors;

use crate::process::{BoundedRun, Command, RunEnding, run_bounded};

/// The reason every probe is refused under `resolution = "static"`.
const STATIC_RESOLUTION_REASON: &str = "resolution = static: toolchains are not run";

/// The reason a probe is refused, or its child killed, once the caller cancelled the read.
const CANCELLED_REASON: &str = "cancelled";

/// How the inputs answer a version probe: whether one runs at all, and how long it may
/// take. Read from the `[dependencies]` table's `resolution` and `command_timeout`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResolutionPolicy {
    collection: rift_protocol::dependencies::DependenciesCollectionConfiguration,
    /// Whether a probe runs. `false` refuses every run before anything spawns.
    execution: bool,
    /// Wall clock one run may take before the inputs kill it.
    command_timeout: Duration,
}

impl From<&DependenciesConfiguration> for ResolutionPolicy {
    fn from(configuration: &DependenciesConfiguration) -> Self {
        Self {
            execution: configuration.resolution == DependencyResolution::Auto,
            collection: configuration.collection,
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
pub(crate) struct FilesystemInputs<'cancel> {
    policy: ResolutionPolicy,
    /// Returns true when the read that owns these inputs should stop: no probe starts
    /// after it does, and a running probe's child is killed.
    cancelled: &'cancel (dyn Fn() -> bool + Sync),
}

impl FilesystemInputs<'static> {
    /// Inputs running the version probes under `policy`, never cancelled.
    pub(crate) const fn new(policy: ResolutionPolicy) -> Self {
        Self {
            policy,
            cancelled: &never_cancelled,
        }
    }
}

impl<'cancel> FilesystemInputs<'cancel> {
    /// Inputs running the version probes under `policy` until `cancelled` answers true.
    pub(crate) const fn cancellable(
        policy: ResolutionPolicy,
        cancelled: &'cancel (dyn Fn() -> bool + Sync),
    ) -> Self {
        Self { policy, cancelled }
    }
}

/// The cancellation of inputs no read cancels.
const fn never_cancelled() -> bool {
    false
}

impl StaticInputs for FilesystemInputs<'_> {
    fn collection(&self) -> rift_protocol::dependencies::DependenciesCollectionConfiguration {
        self.policy.collection
    }

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

impl ContextInputs for FilesystemInputs<'_> {
    /// Runs the program from `PATH` under the policy's bounds.
    ///
    /// The run is cut at the policy's `command_timeout`, or once the inputs' cancellation
    /// answers true, and its streams at `dependencies.collection.toolchain_output`; the
    /// command's environment overlay wins over the inherited value. A program spelled as
    /// a path, every program while the policy runs no toolchain, and every program once
    /// the inputs are cancelled, is refused before anything spawns.
    fn run(&mut self, command: &ToolchainCommand) -> Result<CommandOutput, CommandFailure> {
        let program = command.program;
        if !self.policy.execution {
            return Err(failure(program, STATIC_RESOLUTION_REASON));
        }
        if (self.cancelled)() {
            return Err(failure(program, CANCELLED_REASON));
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
        let run = run_recorded(
            program,
            &mut process,
            self.policy.command_timeout,
            usize::try_from(self.policy.collection.toolchain_output.bytes()).unwrap_or(usize::MAX),
            self.cancelled,
        )
        .map_err(|io| failure(program, format!("failed to launch: {io}")))?;
        output_of(program, run, self.policy.command_timeout)
    }
}

/// Runs one probe through [`run_bounded`] inside one `dependency.probe` operation.
///
/// The operation opens before the spawn and closes after the wait, so its close carries
/// the run's duration. It records the program's file name alone, never the argument
/// vector, the environment, or a path. `run_bounded` hands the child `stdin` null and both
/// output streams piped, so the child inherits none of the server's standard streams.
/// After the run the operation records the child's process identifier `pid`, its
/// `exit_code` when the platform reports one, and an `outcome`: `ok` when the child
/// exited, `timeout` when it was killed at `timeout`, `cancelled` when it was killed once
/// `cancelled` answered true, `error` when it failed to spawn or could not be observed. A
/// killed run records `error.type` beside its `outcome`, so its close prints `✗`.
fn run_recorded(
    program: &str,
    process: &mut Command,
    timeout: Duration,
    capture_bytes: usize,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<BoundedRun> {
    let name = Path::new(program)
        .file_name()
        .map_or(Cow::Borrowed(""), OsStr::to_string_lossy);
    rift_tracing::traced!(
        component = "dependency",
        operation = "dependency.probe",
        program = &*name,
        stdin = "null",
        stdout = "piped",
        stderr = "piped",
        pid = rift_tracing::empty!(),
        exit_code = rift_tracing::empty!(),
        outcome = rift_tracing::empty!(),
        {
            let span = rift_tracing::Span::current();
            let run = run_bounded(process, timeout, capture_bytes, cancelled);
            record_probe(&span, &run);
            run
        }
    )
}

/// Sets the fields a probe operation declared empty from how its run ended.
fn record_probe(span: &rift_tracing::Span, run: &std::io::Result<BoundedRun>) {
    let Ok(run) = run else {
        span.record("outcome", "error");
        return;
    };
    span.record("pid", run.pid);
    match &run.exit {
        _ if run.ending == RunEnding::TimedOut => {
            span.record("outcome", "timeout");
            span.record("error.type", "timeout");
        }
        _ if run.ending == RunEnding::Cancelled => {
            span.record("outcome", "cancelled");
            span.record("error.type", "cancelled");
        }
        Ok(exit) => {
            if let Some(code) = exit.code() {
                span.record("exit_code", code);
            }
            span.record("outcome", "ok");
        }
        Err(_) => {
            span.record("outcome", "error");
        }
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
#[cfg(test)]
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
        _ if run.ending == RunEnding::Cancelled => {
            return Err(failure(program, CANCELLED_REASON));
        }
        _ if run.ending == RunEnding::TimedOut => {
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
///
/// `cancelled` is read before each probe spawns and while each probe runs: once it answers
/// true, no further probe starts and the running one is killed. A read `cancelled` stopped
/// answers the cancellation and no context, so a degraded context that only the
/// cancellation degraded is never cached, published, or logged as degraded.
///
/// # Errors
///
/// Returns `rift.server.read_cancelled` once `cancelled` answered true.
pub(crate) fn read_workspace_context(
    root: &Path,
    visible: &[ProjectPath],
    configured: &[ConfiguredPackage],
    policy: ResolutionPolicy,
    libraries: &[StandardLibrary],
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<DependencyContext, RiftError> {
    let span = rift_tracing::info_span!(
        "dependency.context",
        component = "dependency",
        entries = rift_tracing::empty!(),
        degraded = rift_tracing::empty!(),
    );
    span.in_scope(|| -> Result<DependencyContext, RiftError> {
        if cancelled() {
            return errors::server::read_cancelled().fail();
        }
        let mut inputs = FilesystemInputs::cancellable(policy, cancelled);
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
        if cancelled() {
            return errors::server::read_cancelled().fail();
        }
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
        Ok(context)
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

    fn inputs() -> FilesystemInputs<'static> {
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
    fn dependency_collection_configuration_and_environment_reach_real_inputs() {
        use rift_core::acceptance::{ConfigurationEnvironment, accept_configuration};
        use rift_protocol::configuration::{ByteSize, WorkspaceConfiguration};

        let document = "[dependencies]\nresolution = 'static'\n[dependencies.collection]\nlockfile_size = '32mb'\ntoolchain_output = '128kb'\nmanifests = 257\npackages = 2\ndirectory_entries = 16385\npin_size = '128kb'\nproject_size = '2mb'\nrecord_size = '8mb'\nnesting_depth = 33\n";
        let accepted = accept_configuration::<WorkspaceConfiguration>(
            Some(document),
            &ConfigurationEnvironment::default(),
        )
        .expect("collection table");
        assert_eq!(accepted.configuration().validate(), Ok(()));
        let policy = ResolutionPolicy::from(&accepted.configuration().dependencies);
        let configured = FilesystemInputs::new(policy).collection();
        assert_eq!(configured.lockfile_size, ByteSize::from_bytes(32 << 20));
        assert_eq!(configured.manifests, 257);
        assert_eq!(configured.packages, 2);
        let environment = ConfigurationEnvironment::from_variables([
            ("RIFT_DEPENDENCIES_COLLECTION_LOCKFILE_SIZE", "33mb"),
            ("RIFT_DEPENDENCIES_COLLECTION_TOOLCHAIN_OUTPUT", "129kb"),
            ("RIFT_DEPENDENCIES_COLLECTION_MANIFESTS", "258"),
            ("RIFT_DEPENDENCIES_COLLECTION_PACKAGES", "1"),
            ("RIFT_DEPENDENCIES_COLLECTION_DIRECTORY_ENTRIES", "16386"),
            ("RIFT_DEPENDENCIES_COLLECTION_PIN_SIZE", "129kb"),
            ("RIFT_DEPENDENCIES_COLLECTION_PROJECT_SIZE", "3mb"),
            ("RIFT_DEPENDENCIES_COLLECTION_RECORD_SIZE", "9mb"),
            ("RIFT_DEPENDENCIES_COLLECTION_NESTING_DEPTH", "34"),
        ]);
        let overridden =
            accept_configuration::<WorkspaceConfiguration>(Some(document), &environment)
                .expect("collection variables");
        assert_eq!(overridden.configuration().validate(), Ok(()));
        assert_eq!(overridden.variables().len(), 9);
        let policy = ResolutionPolicy::from(&overridden.configuration().dependencies);
        let actual = FilesystemInputs::new(policy).collection();
        assert_eq!(actual.lockfile_size, ByteSize::from_bytes(33 << 20));
        assert_eq!(actual.toolchain_output, ByteSize::from_bytes(129 << 10));
        assert_eq!(actual.manifests, 258);
        assert_eq!(actual.packages, 1);
        assert_eq!(actual.directory_entries, 16_386);
        assert_eq!(actual.pin_size, ByteSize::from_bytes(129 << 10));
        assert_eq!(actual.project_size, ByteSize::from_bytes(3 << 20));
        assert_eq!(actual.record_size, ByteSize::from_bytes(9 << 20));
        assert_eq!(actual.nesting_depth, 34);

        let directory = tempfile::tempdir().expect("workspace");
        let visible = write_locked_project(directory.path());
        let lockfile_size = fs::metadata(directory.path().join("Cargo.lock"))
            .expect("lockfile")
            .len();
        let mut dependencies = overridden.configuration().dependencies.clone();
        dependencies.collection.packages = 2;
        dependencies.collection.lockfile_size = ByteSize::from_bytes(lockfile_size - 1);
        let low = read_workspace_context(
            directory.path(),
            &visible,
            &[],
            ResolutionPolicy::from(&dependencies),
            &[],
            &|| false,
        )
        .expect("dependency context");
        assert!(low.is_degraded());
        assert!(low.degradations().iter().any(|failure| {
            failure
                .reason
                .contains(&format!("past the {} byte bound", lockfile_size - 1))
        }));
        dependencies.collection.lockfile_size = ByteSize::from_bytes(lockfile_size);
        let exact = read_workspace_context(
            directory.path(),
            &visible,
            &[],
            ResolutionPolicy::from(&dependencies),
            &[],
            &|| false,
        )
        .expect("dependency context");
        assert!(!exact.is_degraded(), "{:?}", exact.degradations());
        assert_eq!(exact.entries().len(), 2);
        let requirement = exact
            .entries()
            .iter()
            .find(|entry| entry.registry.is_none())
            .expect("manifest requirement without registry evidence");
        assert_eq!(requirement.name, "serde");
        assert_eq!(requirement.version, None);
        assert_eq!(requirement.requirement.as_deref(), Some("1"));
        assert_eq!(
            requirement.availability,
            rift_protocol::dependencies::PackageAvailability::RegistryUnresolved
        );
        let locked = exact
            .entries()
            .iter()
            .find(|entry| entry.registry.as_deref() == Some("crates.io"))
            .expect("lockfile defining registry");
        assert_eq!(locked.name, "serde");
        assert_eq!(locked.version.as_deref(), Some("1.0.228"));
        assert_eq!(
            locked.availability,
            rift_protocol::dependencies::PackageAvailability::Canonical
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
    fn configured_probe_output_bound_accepts_exact_and_marks_one_over() {
        let directory = tempfile::tempdir().expect("tempdir");
        let probe = command("sh", &["-c", "printf abc"], directory.path());
        for (maximum, expected, truncated) in [(2, "ab", true), (3, "abc", false)] {
            let mut configuration = DependenciesConfiguration::default();
            configuration.collection.toolchain_output =
                rift_protocol::configuration::ByteSize::from_bytes(maximum);
            let mut inputs = FilesystemInputs::new(ResolutionPolicy::from(&configuration));
            let output = inputs.run(&probe).expect("probe runs");
            assert_eq!(output.stdout, expected);
            assert_eq!(output.stdout_truncated, truncated);
        }
    }

    #[test]
    fn configured_pin_and_project_bounds_reach_standard_library_reads() {
        use rift_protocol::configuration::ByteSize;

        let directory = tempfile::tempdir().expect("workspace");
        let pin = "3.12.3\n";
        fs::write(directory.path().join(".python-version"), pin).expect("pin fixture");
        let project = "{\"engines\":{\"node\":\">=22\"}}";
        fs::write(directory.path().join("package.json"), project).expect("project fixture");
        for exact in [false, true] {
            let mut configuration = DependenciesConfiguration {
                resolution: DependencyResolution::Static,
                ..Default::default()
            };
            let dropped = u64::from(!exact);
            configuration.collection.pin_size =
                ByteSize::from_bytes(u64::try_from(pin.len()).expect("pin size") - dropped);
            configuration.collection.project_size =
                ByteSize::from_bytes(u64::try_from(project.len()).expect("project size") - dropped);
            let mut inputs = FilesystemInputs::new(ResolutionPolicy::from(&configuration));
            let answer = rift_dependency::standard_library_answer(
                &StandardLibraryRequest {
                    root: directory.path(),
                    libraries: &[StandardLibrary::Node, StandardLibrary::Python],
                    execution: false,
                },
                &mut inputs,
            );
            let python = answer
                .entries
                .iter()
                .find(|entry| entry.name == "python")
                .expect("Python library");
            let node = answer
                .entries
                .iter()
                .find(|entry| entry.name == "node")
                .expect("Node library");
            assert_eq!(python.version.as_deref(), exact.then_some("3.12.3"));
            assert_eq!(
                node.requirement.as_deref(),
                Some(if exact { ">=22" } else { ">=0" })
            );
        }
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

    /// The fields of every closed `dependency.probe` operation `run` leaves, in order.
    fn probes_recorded(run: impl FnOnce()) -> Vec<serde_json::Value> {
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("the default filter parses");
        run();
        drop(recorder);
        drain
            .queued_records()
            .iter()
            .filter(|record| record.operation() == "dependency.probe")
            .map(|record| serde_json::from_str::<serde_json::Value>(record.fields()))
            .map(|fields| fields.expect("fields are JSON"))
            .filter(|fields| fields["span"] == "closed")
            .collect()
    }

    /// The process identifier a probe record names; the store keeps every field as text.
    fn pid_of(probe: &serde_json::Value) -> u32 {
        probe["pid"]
            .as_str()
            .and_then(|pid| pid.parse().ok())
            .expect("the record names a pid")
    }

    #[test]
    fn test_probe_of_an_existing_program_records_name_pid_streams_and_exit() {
        let directory = tempfile::tempdir().expect("tempdir");
        let probes = probes_recorded(|| {
            let probe = command("sh", &["-c", "exit 3"], directory.path());
            inputs().run(&probe).expect("sh runs");
        });
        assert_eq!(probes.len(), 1, "{probes:?}");
        let probe = &probes[0];
        assert_eq!(probe["program"], "sh");
        assert_eq!(probe["outcome"], "ok");
        assert_eq!(probe["exit_code"], "3");
        assert!(pid_of(probe) > 0, "{probe}");
        assert_eq!(probe["stdin"], "null");
        assert_eq!(probe["stdout"], "piped");
        assert_eq!(probe["stderr"], "piped");
        assert!(probe["elapsed_ms"].is_string(), "{probe}");
        let text = probe.to_string();
        assert!(!text.contains("exit 3"), "arguments stay out: {text}");
    }

    #[test]
    fn test_probe_of_a_missing_program_records_a_spawn_error_without_a_pid() {
        let directory = tempfile::tempdir().expect("tempdir");
        let probes = probes_recorded(|| {
            let probe = command(
                "rift-test-binary-that-does-not-exist",
                &[],
                directory.path(),
            );
            inputs().run(&probe).expect_err("a missing program fails");
        });
        assert_eq!(probes.len(), 1, "{probes:?}");
        assert_eq!(probes[0]["program"], "rift-test-binary-that-does-not-exist");
        assert_eq!(probes[0]["outcome"], "error");
        assert!(probes[0].get("pid").is_none(), "{}", probes[0]);
        assert!(probes[0].get("exit_code").is_none(), "{}", probes[0]);
    }

    #[test]
    fn test_probe_killed_at_the_timeout_records_the_timeout_outcome() {
        let directory = tempfile::tempdir().expect("tempdir");
        let table = DependenciesConfiguration {
            command_timeout: rift_protocol::configuration::Duration::from_millis(200),
            ..DependenciesConfiguration::default()
        };
        let mut inputs = FilesystemInputs::new(ResolutionPolicy::from(&table));
        let probes = probes_recorded(|| {
            let probe = command("sleep", &["30"], directory.path());
            let failure = inputs.run(&probe).expect_err("sleep overstays");
            assert!(failure.reason.starts_with("overstayed"), "{failure}");
        });
        assert_eq!(probes.len(), 1, "{probes:?}");
        assert_eq!(probes[0]["program"], "sleep");
        assert_eq!(probes[0]["outcome"], "timeout");
        assert_eq!(probes[0]["error.type"], "timeout");
        assert!(pid_of(&probes[0]) > 0, "{}", probes[0]);
    }

    /// A cancellation the wait reads while the probe's child runs kills the child: the
    /// probe answers the cancellation and its close records `outcome` and `error.type`
    /// `cancelled`, the pair the close mark renders as `✗ cancelled`.
    #[test]
    fn test_probe_killed_at_cancellation_records_the_cancelled_outcome() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Reads that answer false: the one before the spawn and one wake of the wait.
        const READS_BEFORE_CANCEL: usize = 2;
        let directory = tempfile::tempdir().expect("tempdir");
        let reads = AtomicUsize::new(0);
        let cancelled = || reads.fetch_add(1, Ordering::Relaxed) >= READS_BEFORE_CANCEL;
        let mut inputs = FilesystemInputs::cancellable(ResolutionPolicy::default(), &cancelled);
        let started = std::time::Instant::now();
        let probes = probes_recorded(|| {
            let probe = command("sleep", &["30"], directory.path());
            let failure = inputs
                .run(&probe)
                .expect_err("a cancelled probe has no output");
            assert_eq!(failure.reason, CANCELLED_REASON, "{failure}");
        });
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the kill must not wait out the sleep: {:?}",
            started.elapsed()
        );
        assert_eq!(probes.len(), 1, "{probes:?}");
        assert_eq!(probes[0]["program"], "sleep");
        assert_eq!(probes[0]["outcome"], "cancelled");
        assert_eq!(probes[0]["error.type"], "cancelled");
        assert!(pid_of(&probes[0]) > 0, "{}", probes[0]);
    }

    /// Once the inputs are cancelled no probe spawns, and no probe operation opens.
    #[test]
    fn test_run_refuses_every_program_once_cancelled_before_spawning() {
        let directory = tempfile::tempdir().expect("tempdir");
        let marker = directory.path().join("ran");
        let touch = format!("touch {}", marker.display());
        let mut inputs = FilesystemInputs::cancellable(ResolutionPolicy::default(), &|| true);
        let probes = probes_recorded(|| {
            let failure = inputs
                .run(&command("sh", &["-c", &touch], directory.path()))
                .expect_err("a cancelled read runs nothing");
            assert_eq!(failure.reason, CANCELLED_REASON);
        });
        assert!(probes.is_empty(), "{probes:?}");
        assert!(!marker.exists(), "the program must not have run");
    }

    /// A context read cancelled once it started answers the cancellation and no context:
    /// every later probe is refused before it spawns, and no degradation the cancellation
    /// caused is logged.
    #[test]
    fn test_context_read_cancelled_after_it_started_answers_no_context() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let directory = tempfile::tempdir().expect("tempdir");
        let visible = write_locked_project(directory.path());
        let reads = AtomicUsize::new(0);
        // The read's own first check passes; every later read answers true.
        let cancelled = || reads.fetch_add(1, Ordering::Relaxed) >= 1;
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("the default filter parses");
        let read = read_workspace_context(
            directory.path(),
            &visible,
            &[],
            ResolutionPolicy::default(),
            &[StandardLibrary::Rust],
            &cancelled,
        );
        drop(recorder);
        let error = read.expect_err("a cancelled read answers no context");
        assert_eq!(error.slug(), errors::server::read_cancelled::SLUG);
        let records = drain.queued_records();
        assert!(
            records
                .iter()
                .all(|record| record.operation() != "dependency.probe"),
            "no probe spawns once cancelled: {records:?}"
        );
        assert!(
            records
                .iter()
                .all(|record| record.message() != "dependency context degraded"),
            "{records:?}"
        );
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
            pid: 0,
            exit: Ok(std::process::ExitStatus::from_raw(9)),
            ending: RunEnding::TimedOut,
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
            pid: 0,
            exit: Err(std::io::Error::other("lost the child")),
            ending: RunEnding::Exited,
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
            &|| false,
        )
        .expect("nothing cancels the read");

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
                "cargo/serde",
                "npm/typescript",
                "stdlib/node",
                "stdlib/python",
                "stdlib/rust"
            ]
        );
        let cargo: Vec<_> = context
            .entries()
            .iter()
            .filter(|entry| entry.manager == "cargo")
            .collect();
        assert_eq!(cargo.len(), 2);
        assert!(cargo.iter().any(|entry| {
            entry.registry.is_none()
                && entry.requirement.as_deref() == Some("1")
                && entry.availability
                    == rift_protocol::dependencies::PackageAvailability::RegistryUnresolved
        }));
        assert!(cargo.iter().any(|entry| {
            entry.registry.as_deref() == Some("crates.io")
                && entry.version.as_deref() == Some("1.0.228")
                && entry.availability == rift_protocol::dependencies::PackageAvailability::Canonical
        }));
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

/// Probe runs every platform makes, through the process runner's test-binary fixture.
#[cfg(test)]
mod fixture_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::process::fixture::{FIXTURE_SLEEP, held_pid, holding_command, kill_pid};

    /// A cancellation that lands while the probe's child left a process holding both
    /// output pipes: the probe still closes within the runner's bound, recording
    /// `outcome` and `error.type` `cancelled`, and answers the cancellation.
    #[test]
    fn test_probe_cancelled_while_its_child_holds_the_pipes_records_the_cancellation() {
        /// The runner's bound after a cancellation, plus the fixture's own start and
        /// scheduling slack; far below the fixture's sleep.
        const CANCEL_LATENCY_MAX: Duration = Duration::from_secs(10);
        let directory = tempfile::tempdir().expect("tempdir");
        let pid_file = directory.path().join("held.pid");
        let fired = AtomicBool::new(false);
        let cancelled = || {
            if pid_file.exists() {
                fired.store(true, Ordering::Relaxed);
            }
            fired.load(Ordering::Relaxed)
        };
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("the default filter parses");
        let started = std::time::Instant::now();
        let run = run_recorded(
            "fixture",
            &mut holding_command(&pid_file),
            FIXTURE_SLEEP,
            toolchain_capture_bytes(),
            &cancelled,
        )
        .expect("the fixture launches");
        let elapsed = started.elapsed();
        drop(recorder);
        if let Some(pid) = held_pid(&pid_file) {
            kill_pid(pid);
        }

        assert!(
            elapsed < CANCEL_LATENCY_MAX,
            "the probe must not wait on the held pipes: {elapsed:?}"
        );
        let failure =
            output_of("fixture", run, FIXTURE_SLEEP).expect_err("a cancelled probe has no output");
        assert_eq!(failure.reason, CANCELLED_REASON, "{failure}");
        let probes: Vec<serde_json::Value> = drain
            .queued_records()
            .iter()
            .filter(|record| record.operation() == "dependency.probe")
            .map(|record| serde_json::from_str(record.fields()).expect("fields are JSON"))
            .filter(|fields: &serde_json::Value| fields["span"] == "closed")
            .collect();
        assert_eq!(probes.len(), 1, "{probes:?}");
        assert_eq!(probes[0]["outcome"], "cancelled");
        assert_eq!(probes[0]["error.type"], "cancelled");
    }
}
