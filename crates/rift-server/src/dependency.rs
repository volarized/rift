//! The server's filesystem-backed inspector, and workspace dependency resolution through it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rift_dependency::{
    CommandFailure, CommandOutput, DependencyCatalog, FileObservation, Inspector,
    TOOLCHAIN_COMMAND_TIMEOUT, TOOLCHAIN_OUTPUT_BYTES_MAX, ToolchainCommand,
};
use rift_protocol::read::ProjectPath;

use crate::process::{BoundedRun, run_bounded};

/// The inspector that answers from this machine: its files, environment, and `PATH`.
#[derive(Debug)]
pub(crate) struct FilesystemInspector;

impl Inspector for FilesystemInspector {
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

    /// Runs the program from `PATH` under the crate's toolchain bounds.
    ///
    /// The run is cut at [`TOOLCHAIN_COMMAND_TIMEOUT`] and its standard output
    /// at [`TOOLCHAIN_OUTPUT_BYTES_MAX`]. A program spelled as a path is
    /// refused before anything spawns.
    fn run(&mut self, command: &ToolchainCommand) -> Result<CommandOutput, CommandFailure> {
        let program = command.program;
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
            TOOLCHAIN_COMMAND_TIMEOUT,
            toolchain_capture_bytes(),
        )
        .map_err(|io| failure(program, format!("failed to launch: {io}")))?;
        output_of(program, run)
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

/// The run's output, or the failure that left it without one.
fn output_of(program: &str, run: BoundedRun) -> Result<CommandOutput, CommandFailure> {
    let exit = match run.exit {
        _ if run.timed_out => {
            let seconds = TOOLCHAIN_COMMAND_TIMEOUT.as_secs();
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
/// Every file read and toolchain run goes through [`FilesystemInspector`]. The
/// span records the entry count and whether the answer degraded, and each
/// degradation is logged once as a warning.
pub(crate) fn resolve_workspace_catalog(root: &Path, visible: &[ProjectPath]) -> DependencyCatalog {
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
        &mut FilesystemInspector,
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

    #[test]
    fn test_read_file_answers_absent_bytes_and_over_bound() {
        let directory = tempfile::tempdir().expect("tempdir");
        let manifest = directory.path().join("Cargo.toml");
        std::fs::write(&manifest, b"[package]\n").expect("write manifest");
        let mut inspector = FilesystemInspector;
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
        let mut inspector = FilesystemInspector;
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
        let mut inspector = FilesystemInspector;
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
        let mut inspector = FilesystemInspector;
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
        let mut inspector = FilesystemInspector;
        let probe = command("sh", &["-c", "printf err >&2; exit 3"], directory.path());
        let output = inspector.run(&probe).expect("sh runs");
        assert_eq!(output.exit_code, Some(3));
        assert_eq!(output.stderr, "err");
        assert!(!output.succeeded());
    }

    #[test]
    fn test_run_starts_in_the_working_directory() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut inspector = FilesystemInspector;
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
        let mut inspector = FilesystemInspector;
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
        let mut inspector = FilesystemInspector;
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

    #[test]
    fn test_run_inherits_the_server_environment() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut inspector = FilesystemInspector;
        let probe = command("sh", &["-c", r#"printf "%s" "$PATH""#], directory.path());
        let output = inspector.run(&probe).expect("sh runs");
        let expected = std::env::var("PATH").expect("the test process has a PATH");
        assert_eq!(output.stdout, expected);
    }

    #[test]
    fn test_environment_and_home_directory_answer_from_the_process() {
        let mut inspector = FilesystemInspector;
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

        let catalog = resolve_workspace_catalog(root, &visible);

        let inputs: Vec<&str> = catalog.inputs().map(|path| path.0.as_str()).collect();
        assert_eq!(
            inputs,
            ["Cargo.lock", "Cargo.toml"],
            "{:?}",
            catalog.degradations()
        );
        assert!(!catalog.is_degraded(), "{:?}", catalog.degradations());
    }

    #[test]
    fn test_resolve_workspace_catalog_without_manifests_is_empty() {
        let directory = tempfile::tempdir().expect("tempdir");
        let visible = [ProjectPath("src/lib.rs".to_owned())];
        let catalog = resolve_workspace_catalog(directory.path(), &visible);
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

        let failure = output_of("cargo", run).expect_err("a killed run has no output");

        assert_eq!(failure.program, "cargo");
        assert!(failure.reason.starts_with("overstayed "), "{failure}");
        assert!(failure.reason.ends_with("s and was killed"), "{failure}");
    }

    #[test]
    fn test_output_of_reports_an_unobservable_run_with_the_io_text() {
        let run = BoundedRun {
            exit: Err(std::io::Error::other("lost the child")),
            timed_out: false,
            stdout: rift_core::CapturedStream::default(),
            stderr: rift_core::CapturedStream::default(),
        };

        let failure = output_of("cargo", run).expect_err("an unobserved run has no output");

        assert_eq!(failure.reason, "waiting on the process: lost the child");
    }
}
