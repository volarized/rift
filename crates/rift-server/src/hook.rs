//! Runs the workspace's configured hooks after a change applies.
//!
//! Each hook is an executable started directly — no shell — inside the
//! changed tree, its streams captured up to the configured prefix, its
//! wall-clock bounded by `timeout_ms`. A command starts from the environment
//! the server inherited, with the hook's `environment` entries laid on top.
//! Hooks observe an already-applied change: a failing hook rides the result
//! as evidence and never rolls the change back.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rift_protocol::configuration::{ChangedPaths, CommandHook};
use rift_protocol::read::ProjectPath;

/// How long the runner sleeps between checks on a running hook. The wait
/// loop wakes at most `timeout_ms / HOOK_POLL_INTERVAL + 1` times.
const HOOK_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Bytes read from a hook stream per read call.
const STREAM_READ_BYTES: usize = 8 << 10;

/// Bytes of one hook stream the runner counts before it stops reading. A
/// hook that produces more blocks on its full pipe until `timeout_ms` kills
/// it, and the reported total stays at this ceiling.
const STREAM_TOTAL_BYTES_MAX: u64 = 64 << 20;

/// What one configured hook's run produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookRun {
    /// The configured hook id.
    pub id: String,
    /// How the run ended.
    pub status: HookStatus,
    /// The process exit code, where the platform reported one.
    pub exit_code: Option<i32>,
    /// Captured standard output, bounded by `output_limit_bytes`.
    pub stdout: CapturedStream,
    /// Captured standard error, bounded by `output_limit_bytes`.
    pub stderr: CapturedStream,
}

/// How one hook run ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookStatus {
    /// The process exited zero.
    Passed,
    /// The process exited nonzero.
    Failed,
    /// The process overstayed `timeout_ms` and was killed.
    TimedOut,
    /// The hook never produced a verdict: it was refused or failed to
    /// launch or be observed.
    Error(String),
}

/// One captured output stream: a bounded prefix plus the full size, so a
/// truncated log is distinguishable from a short one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapturedStream {
    /// The captured prefix, lossily decoded as UTF-8.
    pub text: String,
    /// Bytes of the prefix actually captured.
    pub captured_bytes: u64,
    /// Bytes the stream produced, counted up to the runner's drain ceiling.
    pub total_bytes: u64,
    /// Whether the capture stopped short of the full stream.
    pub truncated: bool,
}

/// Runs every configured hook inside the changed tree, in list order, over
/// the byte-ordered changed paths. The work is bounded by configuration:
/// at most the configured hook count, each killed at its `timeout_ms`.
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
        .map(|hook| run_one(hook, tree_root, &ordered))
        .collect()
}

/// Runs one hook to completion, killing it at `timeout_ms`.
fn run_one(hook: &CommandHook, tree_root: &Path, ordered_paths: &[&str]) -> HookRun {
    let error = |message: String| HookRun {
        id: hook.id.clone(),
        status: HookStatus::Error(message),
        exit_code: None,
        stdout: CapturedStream::default(),
        stderr: CapturedStream::default(),
    };
    if hook.program.is_empty() {
        return error("empty program".to_owned());
    }
    if Path::new(&hook.program).is_absolute() {
        return error(format!(
            "absolute executable path refused: {}",
            hook.program
        ));
    }

    let working_directory = if hook.working_directory.0.is_empty() {
        tree_root.to_path_buf()
    } else {
        tree_root.join(&hook.working_directory.0)
    };
    let mut command = Command::new(&hook.program);
    command.args(&hook.arguments);
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
        Err(io) => return error(format!("failed to launch: {io}")),
    };
    #[expect(
        clippy::cast_possible_truncation,
        reason = "output_limit_bytes is validated to at most 4096, which fits usize on every \
                  served target"
    )]
    let capture_bytes = hook.output_limit_bytes as usize;
    let stdout_drain = drain_thread(child.stdout.take(), capture_bytes);
    let stderr_drain = drain_thread(child.stderr.take(), capture_bytes);

    let (exit, timed_out) = wait_bounded(&mut child, Duration::from_millis(hook.timeout_ms));
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
    use rift_protocol::configuration::{Determinism, HookKind, HookType};
    use std::collections::BTreeMap;

    fn hook(program: &str, arguments: &[&str]) -> CommandHook {
        CommandHook {
            r#type: HookType::Command,
            id: "probe".to_owned(),
            kind: HookKind::Other,
            program: program.to_owned(),
            arguments: arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
            changed_paths: ChangedPaths::None,
            working_directory: ProjectPath(String::new()),
            environment: BTreeMap::new(),
            timeout_ms: 10_000,
            output_limit_bytes: 4_096,
            guarantees: Vec::new(),
            determinism: Determinism::Deterministic,
        }
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
        let runs = run_hooks(&[echo], directory.path(), &changed);
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.status, HookStatus::Passed);
        assert_eq!(run.exit_code, Some(0));
        assert_eq!(run.stdout.text, "hello a.rs pkg/a.rs pkg/b.rs\n");
        assert!(!run.stdout.truncated);
        assert_eq!(run.stdout.total_bytes, run.stdout.captured_bytes);
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
        slow.timeout_ms = 200;
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
        noisy.output_limit_bytes = 256;
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
