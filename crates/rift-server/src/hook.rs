//! Runs the workspace's configured hooks after a change applies.
//!
//! Each hook is an executable started directly - no shell - inside the
//! changed tree, its streams captured up to the configured prefix, its
//! wall-clock bounded by `timeout`. A command starts from the environment
//! the server inherited, with the hook's `environment` entries laid on top.
//! [`run_hooks`] selects commands against the initially changed project paths.
//! Hooks observe an already-applied change: a failing hook rides the result
//! as evidence and never rolls the change back.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rift_core::{
    CapturedStream, ProjectPath as CoreProjectPath, STREAM_READ_BYTES, STREAM_TOTAL_BYTES_MAX,
};
use rift_index::{PathMatcher, WorkspaceIndexError};
use rift_protocol::configuration::{ChangedPaths, CommandHook};
use rift_protocol::read::ProjectPath;

/// How long the runner sleeps between checks on a running hook. The wait
/// loop wakes at most `timeout / HOOK_POLL_INTERVAL + 1` times.
const HOOK_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// What one configured hook's run produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookRun {
    /// The configured hook id.
    pub id: String,
    /// How the run ended.
    pub status: HookStatus,
    /// The process exit code, where the platform reported one.
    pub exit_code: Option<i32>,
    /// Captured standard output, bounded by `output_limit`.
    pub stdout: CapturedStream,
    /// Captured standard error, bounded by `output_limit`.
    pub stderr: CapturedStream,
}

/// How one hook run ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookStatus {
    /// The process exited zero.
    Passed,
    /// The process exited nonzero.
    Failed,
    /// The process overstayed `timeout` and was killed.
    TimedOut,
    /// The hook never produced a verdict: it was refused or failed to
    /// launch or be observed.
    Error(String),
}

/// Runs each selected hook inside the changed tree, in list order.
///
/// Selection uses the initially changed paths. Each selected hook runs once
/// over the same byte-ordered path list. An invalid pattern produces an error
/// run for its hook. Work is bounded by the configured hook count and each
/// hook's `timeout`.
#[must_use]
pub fn run_hooks(
    hooks: &[CommandHook],
    tree_root: &Path,
    changed_paths: &[ProjectPath],
) -> Vec<HookRun> {
    let mut ordered: Vec<&str> = changed_paths.iter().map(|path| path.0.as_str()).collect();
    ordered.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    hooks
        .iter()
        .filter_map(
            |hook| match hook_matches_paths(hook, tree_root, changed_paths) {
                Ok(true) => Some(run_one(hook, tree_root, &ordered)),
                Ok(false) => None,
                Err(error) => Some(error_run(
                    hook,
                    format!("hook path selection failed: {error}"),
                )),
            },
        )
        .collect()
}

/// Returns whether one hook selects any initially changed project path.
///
/// An empty include and exclude pair selects every change. Otherwise, the
/// include patterns select candidates and the exclude patterns remove them.
///
/// # Errors
///
/// Returns [`WorkspaceIndexError`] when an include or exclude pattern is not
/// a valid glob.
pub fn hook_matches_paths(
    hook: &CommandHook,
    tree_root: &Path,
    changed_paths: &[ProjectPath],
) -> Result<bool, WorkspaceIndexError> {
    if hook.include.is_empty() && hook.exclude.is_empty() {
        return Ok(true);
    }
    let include: Vec<String> = hook
        .include
        .iter()
        .map(|pattern| pattern.0.clone())
        .collect();
    let exclude: Vec<String> = hook
        .exclude
        .iter()
        .map(|pattern| pattern.0.clone())
        .collect();
    let matcher = PathMatcher::build(tree_root, &include, &exclude)?;
    Ok(changed_paths
        .iter()
        .any(|path| matcher.includes(&tree_root.join(&path.0))))
}

/// Runs one configured hook inside changed tree.
#[must_use]
pub fn run_hook(hook: &CommandHook, tree_root: &Path, changed_paths: &[ProjectPath]) -> HookRun {
    let mut ordered: Vec<&str> = changed_paths.iter().map(|path| path.0.as_str()).collect();
    ordered.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    run_one(hook, tree_root, &ordered)
}

/// Runs one hook to completion, killing it at `timeout`.
fn run_one(hook: &CommandHook, tree_root: &Path, ordered_paths: &[&str]) -> HookRun {
    let program = hook.command.program();
    if program.is_empty() {
        return error_run(hook, "empty program".to_owned());
    }
    if Path::new(program).is_absolute() {
        return error_run(hook, format!("absolute executable path refused: {program}"));
    }
    if has_dot_segment(program) {
        return error_run(
            hook,
            format!("executable path segment escapes the workspace: {program}"),
        );
    }
    let Ok(working_directory) = CoreProjectPath::new(hook.working_directory.0.clone()) else {
        return error_run(
            hook,
            format!(
                "working directory escapes the workspace: {}",
                hook.working_directory.0
            ),
        );
    };
    let working_directory = if working_directory.as_str().is_empty() {
        tree_root.to_path_buf()
    } else {
        tree_root.join(working_directory.as_str())
    };
    let mut command = Command::new(program);
    command.args(hook.command.arguments());
    if hook.changed_paths == ChangedPaths::Append {
        command.args(ordered_paths);
    }
    command
        .current_dir(working_directory)
        .envs(&hook.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(io) => return error_run(hook, format!("failed to launch: {io}")),
    };
    #[expect(
        clippy::cast_possible_truncation,
        reason = "output_limit is validated to at most 4096 bytes, which fits usize on every \
                  served target"
    )]
    let capture_bytes = hook.output_limit.bytes() as usize;
    let stdout_drain = drain_thread(child.stdout.take(), capture_bytes);
    let stderr_drain = drain_thread(child.stderr.take(), capture_bytes);

    let (exit, timed_out) = wait_bounded(
        &mut child,
        Duration::from_millis(hook.timeout.milliseconds()),
    );
    let stdout = join_drain(stdout_drain);
    let stderr = join_drain(stderr_drain);

    let (status, exit_code) = conclude(exit, timed_out);
    HookRun {
        id: hook.id.clone(),
        status,
        exit_code,
        stdout,
        stderr,
    }
}

/// Builds one hook run that ended before process execution.
fn error_run(hook: &CommandHook, message: String) -> HookRun {
    HookRun {
        id: hook.id.clone(),
        status: HookStatus::Error(message),
        exit_code: None,
        stdout: CapturedStream::default(),
        stderr: CapturedStream::default(),
    }
}

/// Whether `program` carries a `.` or `..` segment, which could resolve
/// outside `working_directory`. Configuration acceptance refuses this
/// already; this check catches a `rift.toml` edited on disk after
/// acceptance, which acceptance never saw.
fn has_dot_segment(program: &str) -> bool {
    program
        .split('/')
        .any(|segment| matches!(segment, "." | ".."))
}

/// The run's verdict from how the wait ended.
fn conclude(exit: std::io::Result<ExitStatus>, timed_out: bool) -> (HookStatus, Option<i32>) {
    match exit {
        _ if timed_out => (HookStatus::TimedOut, None),
        Ok(exit) if exit.success() => (HookStatus::Passed, exit.code()),
        Ok(exit) => (HookStatus::Failed, exit.code()),
        Err(io) => (HookStatus::Error(format!("waiting on hook: {io}")), None),
    }
}

/// Waits for the child within `timeout`, then kills it. The poll loop wakes
/// at most `timeout / HOOK_POLL_INTERVAL + 1` times before the deadline
/// forces the kill.
fn wait_bounded(child: &mut Child, timeout: Duration) -> (std::io::Result<ExitStatus>, bool) {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(exit)) => return (Ok(exit), false),
            Ok(None) => {}
            Err(io) => {
                // The child's state is unknown, and an unobservable child must
                // not outlive its bound: kill it before reporting the failure.
                let _ = child.kill();
                let _ = child.wait();
                return (Err(io), false);
            }
        }
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
        else {
            let _ = child.kill();
            return (child.wait(), true);
        };
        std::thread::sleep(remaining.min(HOOK_POLL_INTERVAL));
    }
}

/// Starts one reader thread over a hook stream. Null where the pipe was
/// not handed over, which try-spawned children never do.
fn drain_thread(
    stream: Option<impl Read + Send + 'static>,
    capture_bytes: usize,
) -> Option<std::thread::JoinHandle<CapturedStream>> {
    let stream = stream?;
    Some(std::thread::spawn(move || drain(stream, capture_bytes)))
}

/// The thread's captured stream; a reader that panicked reports an empty
/// capture rather than tearing down the change result.
fn join_drain(handle: Option<std::thread::JoinHandle<CapturedStream>>) -> CapturedStream {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

/// Reads one stream until end-of-file or the drain ceiling, keeping the
/// first `capture_bytes` and counting the rest. The loop is bounded by
/// [`STREAM_TOTAL_BYTES_MAX`]: each read returns at least one byte, so it
/// iterates at most that many times before end-of-file, an error, or the
/// ceiling stops it.
fn drain(mut stream: impl Read, capture_bytes: usize) -> CapturedStream {
    let mut kept: Vec<u8> = Vec::with_capacity(capture_bytes.min(STREAM_READ_BYTES));
    let mut total_bytes: u64 = 0;
    let mut buffer = [0_u8; STREAM_READ_BYTES];
    while total_bytes < STREAM_TOTAL_BYTES_MAX {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read_bytes) => {
                total_bytes = STREAM_TOTAL_BYTES_MAX.min(total_bytes + read_bytes as u64);
                if kept.len() < capture_bytes {
                    let taken = read_bytes.min(capture_bytes - kept.len());
                    kept.extend_from_slice(&buffer[..taken]);
                }
            }
        }
    }
    CapturedStream {
        text: String::from_utf8_lossy(&kept).into_owned(),
        captured_bytes: kept.len() as u64,
        total_bytes,
        truncated: total_bytes > kept.len() as u64,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use rift_protocol::configuration as hook_configuration;
    use rift_protocol::configuration::{CommandInput, Determinism, HookKind};
    use rift_protocol::read::PathPattern;
    use std::collections::BTreeMap;

    fn hook(program: &str, arguments: &[&str]) -> CommandHook {
        let command = std::iter::once(program)
            .chain(arguments.iter().copied())
            .map(str::to_owned)
            .collect();
        CommandHook {
            id: "probe".to_owned(),
            kind: HookKind::Other,
            command: CommandInput::ProgramAndArguments(command),
            changed_paths: ChangedPaths::None,
            writes: hook_configuration::HookWrites::None,
            working_directory: ProjectPath(String::new()),
            environment: BTreeMap::new(),
            timeout: hook_configuration::Duration::from_millis(10_000),
            output_limit: hook_configuration::ByteSize::from_bytes(4_096),
            failure_severity: hook_configuration::HookFailureSeverity::Error,
            guarantees: Vec::new(),
            determinism: Determinism::Deterministic,
            include: Vec::new(),
            exclude: Vec::new(),
        }
    }

    fn pattern(value: &str) -> PathPattern {
        PathPattern(value.to_owned())
    }

    /// The message of an error status, empty for any other.
    fn error_text(status: &HookStatus) -> &str {
        match status {
            HookStatus::Error(message) => message,
            _ => "",
        }
    }

    fn paths(raw: &[&str]) -> Vec<ProjectPath> {
        raw.iter()
            .map(|path| ProjectPath((*path).to_owned()))
            .collect()
    }

    #[test]
    fn test_passing_hook_appends_changed_paths_byte_ordered() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut echo = hook("echo", &["hello"]);
        echo.changed_paths = ChangedPaths::Append;
        let changed = paths(&["pkg/b.rs", "a.rs", "pkg/a.rs"]);
        let run = run_hook(&echo, directory.path(), &changed);
        assert_eq!(run.status, HookStatus::Passed);
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(run.stdout.text, "hello a.rs pkg/a.rs pkg/b.rs\n");
        assert!(!run.stdout.truncated);
        assert_eq!(run.stdout.total_bytes, run.stdout.captured_bytes);
    }

    #[test]
    fn test_string_and_list_commands_run_directly() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut string = hook("true", &[]);
        string.command = CommandInput::Program("true".to_owned());
        let list = hook("true", &[]);
        let runs = run_hooks(&[string, list], directory.path(), &[]);
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().all(|run| run.status == HookStatus::Passed));
    }

    #[test]
    fn test_command_argument_is_literal_shell_text() {
        let directory = tempfile::tempdir().expect("tempdir");
        let marker = directory.path().join("shell-ran");
        let literal = format!("$HOME; touch {}", marker.display());
        let run = run_hook(&hook("printf", &["%s", &literal]), directory.path(), &[]);
        assert_eq!(run.status, HookStatus::Passed);
        assert_eq!(run.stdout.text, literal);
        assert!(!marker.exists(), "the command must not start a shell");
    }

    #[test]
    fn test_hook_include_selects_matching_path() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut selected = hook("true", &[]);
        selected.include = vec![pattern("src/**")];
        let runs = run_hooks(&[selected], directory.path(), &paths(&["src/lib.rs"]));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, HookStatus::Passed);
    }

    #[test]
    fn test_hook_exclude_removes_matching_paths() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut excluded = hook("true", &[]);
        excluded.exclude = vec![pattern("generated/**")];
        let runs = run_hooks(&[excluded], directory.path(), &paths(&["generated/lib.rs"]));
        assert!(runs.is_empty());
    }

    #[test]
    fn test_hook_with_unrelated_include_is_skipped() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut rust = hook("true", &[]);
        rust.include = vec![pattern("**/*.rs")];
        let runs = run_hooks(&[rust], directory.path(), &paths(&["Cargo.toml"]));
        assert!(runs.is_empty());
    }

    #[test]
    fn test_multi_file_change_runs_selected_hook_once() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut rust = hook("true", &[]);
        rust.include = vec![pattern("src/**")];
        let changed = paths(&["src/lib.rs", "src/main.rs", "README.md"]);
        let runs = run_hooks(&[rust], directory.path(), &changed);
        assert_eq!(runs.len(), 1);
    }

    #[test]
    fn test_empty_include_selects_every_change() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runs = run_hooks(
            &[hook("true", &[])],
            directory.path(),
            &paths(&["notes.unknown"]),
        );
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, HookStatus::Passed);
    }

    #[test]
    fn test_invalid_hook_pattern_returns_error_without_panicking() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut invalid = hook("true", &[]);
        invalid.include = vec![pattern("[")];
        let runs = run_hooks(&[invalid], directory.path(), &paths(&["src/lib.rs"]));
        assert_eq!(runs.len(), 1);
        let status = &runs[0].status;
        assert!(
            error_text(status).contains("path selection failed"),
            "{status:?}"
        );
    }

    #[test]
    fn test_nonzero_exit_fails_with_its_code() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runs = run_hooks(&[hook("false", &[])], directory.path(), &[]);
        assert_eq!(runs[0].status, HookStatus::Failed);
        assert_eq!(runs[0].exit_code, Some(1));
    }

    #[test]
    fn test_timeout_kills_the_hook_without_waiting_it_out() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut slow = hook("sleep", &["10"]);
        slow.timeout = hook_configuration::Duration::from_millis(200);
        let started = Instant::now();
        let runs = run_hooks(&[slow], directory.path(), &[]);
        assert_eq!(runs[0].status, HookStatus::TimedOut);
        assert_eq!(runs[0].exit_code, None);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "the kill must not wait out the sleep: {elapsed:?}"
        );
    }

    #[test]
    fn test_output_is_capped_and_full_size_reported() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut noisy = hook("seq", &["1", "5000"]);
        noisy.output_limit = hook_configuration::ByteSize::from_bytes(256);
        let runs = run_hooks(&[noisy], directory.path(), &[]);
        let stdout = &runs[0].stdout;
        assert_eq!(stdout.captured_bytes, 256);
        assert!(stdout.total_bytes > 256, "total {}", stdout.total_bytes);
        assert!(stdout.truncated);
        assert_eq!(runs[0].status, HookStatus::Passed);
    }

    #[test]
    fn test_working_directory_and_environment_shape_the_process() {
        let directory = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(directory.path().join("sub")).expect("create sub");
        let mut in_sub = hook("pwd", &[]);
        in_sub.working_directory = ProjectPath("sub".to_owned());
        let mut with_environment = hook("printenv", &["RIFT_HOOK_PROBE"]);
        with_environment
            .environment
            .insert("RIFT_HOOK_PROBE".to_owned(), "42".to_owned());
        let runs = run_hooks(&[in_sub, with_environment], directory.path(), &[]);
        let stdout = runs[0].stdout.text.trim_end();
        assert!(stdout.ends_with("/sub"), "{stdout}");
        assert_eq!(runs[1].stdout.text, "42\n");
    }

    #[test]
    fn test_missing_program_is_an_error_not_a_panic() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runs = run_hooks(
            &[hook("rift-test-binary-that-does-not-exist", &[])],
            directory.path(),
            &[],
        );
        let status = &runs[0].status;
        assert!(error_text(status).contains("launch"), "{status:?}");
    }

    #[test]
    fn test_absolute_program_is_refused_before_spawning() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runs = run_hooks(&[hook("/bin/echo", &["hi"])], directory.path(), &[]);
        let status = &runs[0].status;
        assert!(error_text(status).contains("absolute"), "{status:?}");
    }

    #[test]
    fn test_empty_program_is_an_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runs = run_hooks(&[hook("", &[])], directory.path(), &[]);
        let status = &runs[0].status;
        assert!(error_text(status).contains("program"), "{status:?}");
    }

    #[test]
    fn test_bare_program_name_is_accepted() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runs = run_hooks(&[hook("echo", &["hi"])], directory.path(), &[]);
        assert_eq!(runs[0].status, HookStatus::Passed);
    }

    #[test]
    fn test_project_relative_program_below_the_root_is_accepted() {
        let directory = tempfile::tempdir().expect("tempdir");
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin).expect("create bin");
        let script = bin.join("run.sh");
        std::fs::write(&script, "#!/bin/sh\necho below-root\n").expect("write script");
        let mut permissions = std::fs::metadata(&script)
            .expect("script metadata")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&script, permissions).expect("set script executable");
        let runs = run_hooks(&[hook("bin/run.sh", &[])], directory.path(), &[]);
        assert_eq!(runs[0].status, HookStatus::Passed, "{:?}", runs[0].status);
    }

    #[test]
    fn test_program_dot_segment_is_refused_before_spawning() {
        let directory = tempfile::tempdir().expect("tempdir");
        for program in ["../evil", "sub/../evil", "./evil"] {
            let runs = run_hooks(&[hook(program, &[])], directory.path(), &[]);
            let status = &runs[0].status;
            assert!(
                error_text(status).contains("escapes the workspace"),
                "{program}: {status:?}"
            );
        }
    }

    #[test]
    fn test_absolute_working_directory_is_refused_before_spawning() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut escaping = hook("echo", &[]);
        escaping.working_directory = ProjectPath("/etc".to_owned());
        let runs = run_hooks(&[escaping], directory.path(), &[]);
        let status = &runs[0].status;
        assert!(
            error_text(status).contains("working directory"),
            "{status:?}"
        );
    }

    #[test]
    fn test_working_directory_dot_segment_is_refused_before_spawning() {
        let directory = tempfile::tempdir().expect("tempdir");
        for working_directory in ["..", "../outside", "scripts/../outside"] {
            let mut escaping = hook("echo", &[]);
            escaping.working_directory = ProjectPath(working_directory.to_owned());
            let runs = run_hooks(&[escaping], directory.path(), &[]);
            let status = &runs[0].status;
            assert!(
                error_text(status).contains("working directory"),
                "{working_directory}: {status:?}"
            );
        }
    }

    #[test]
    fn test_empty_working_directory_is_still_the_root() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut at_root = hook("pwd", &[]);
        at_root.working_directory = ProjectPath(String::new());
        let runs = run_hooks(&[at_root], directory.path(), &[]);
        assert_eq!(runs[0].status, HookStatus::Passed);
        let printed = runs[0].stdout.text.trim_end();
        let canonical_root =
            std::fs::canonicalize(directory.path()).expect("temporary directory must resolve");
        assert_eq!(
            std::fs::canonicalize(printed).expect("printed directory must resolve"),
            canonical_root
        );
    }

    #[test]
    fn test_hook_inherits_the_server_environment() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runs = run_hooks(&[hook("printenv", &["PATH"])], directory.path(), &[]);
        assert_eq!(runs[0].status, HookStatus::Passed);
        assert!(
            !runs[0].stdout.text.trim().is_empty(),
            "the child must see the server's PATH"
        );
    }

    #[test]
    fn test_configured_environment_wins_over_the_inherited_value() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut overlaid = hook("printenv", &["HOME"]);
        overlaid
            .environment
            .insert("HOME".to_owned(), "/rift/overlay".to_owned());
        let runs = run_hooks(&[overlaid], directory.path(), &[]);
        assert_eq!(runs[0].stdout.text, "/rift/overlay\n");
    }

    /// A stream that never ends, for proving the drain ceiling.
    struct EndlessStream;

    impl Read for EndlessStream {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            buffer.fill(b'x');
            Ok(buffer.len())
        }
    }

    #[test]
    fn test_drain_stops_counting_at_the_stream_ceiling() {
        let captured = drain(EndlessStream, 16);
        assert_eq!(captured.total_bytes, STREAM_TOTAL_BYTES_MAX);
        assert_eq!(captured.captured_bytes, 16);
        assert!(captured.truncated);
    }

    /// A stream that fails on its first read.
    struct FailingStream;

    impl Read for FailingStream {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("stream torn down"))
        }
    }

    #[test]
    fn test_drain_reports_an_erroring_stream_as_empty() {
        let captured = drain(FailingStream, 16);
        assert_eq!(captured, CapturedStream::default());
    }

    /// A stream whose reader thread panics, for proving the join fallback.
    struct PanickingStream;

    impl Read for PanickingStream {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("reader died mid-stream");
        }
    }

    #[test]
    fn test_panicked_reader_reports_an_empty_capture() {
        let captured = join_drain(drain_thread(Some(PanickingStream), 16));
        assert_eq!(captured, CapturedStream::default());
    }

    #[test]
    fn test_conclude_maps_every_wait_outcome() {
        use std::os::unix::process::ExitStatusExt as _;
        let success = ExitStatus::from_raw(0);
        let failure = ExitStatus::from_raw(2 << 8);
        assert_eq!(conclude(Ok(success), false), (HookStatus::Passed, Some(0)));
        assert_eq!(conclude(Ok(failure), false), (HookStatus::Failed, Some(2)));
        assert_eq!(conclude(Ok(success), true), (HookStatus::TimedOut, None));
        let (status, exit_code) = conclude(Err(std::io::Error::other("wait torn down")), false);
        assert_eq!(exit_code, None);
        assert_eq!(error_text(&status), "waiting on hook: wait torn down");
        assert_eq!(error_text(&HookStatus::Passed), "");
    }

    #[test]
    fn test_hooks_run_in_list_order() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut first = hook("echo", &["first"]);
        first.id = "first".to_owned();
        let mut second = hook("echo", &["second"]);
        second.id = "second".to_owned();
        let runs = run_hooks(&[first, second], directory.path(), &[]);
        assert_eq!(runs[0].id, "first");
        assert_eq!(runs[1].id, "second");
    }
}
