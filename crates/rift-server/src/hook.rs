//! Runs the workspace's configured hooks after a change applies.
//!
//! Each hook is an executable started directly — no shell — inside the
//! changed tree, its streams captured up to the configured prefix, its
//! wall-clock bounded by `timeout_ms`. Hooks observe an already-applied
//! change: a failing hook rides the result as evidence and never rolls the
//! change back.

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
    /// Bytes the stream produced in total.
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
    let Some((program, literal_arguments)) = hook.argv.split_first() else {
        return error("empty argv".to_owned());
    };
    if Path::new(program).is_absolute() {
        return error(format!("absolute executable path refused: {program}"));
    }

    let working_directory = if hook.working_directory.0.is_empty() {
        tree_root.to_path_buf()
    } else {
        tree_root.join(&hook.working_directory.0)
    };
    let mut command = Command::new(program);
    command.args(literal_arguments);
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

    let (status, exit_code) = match exit {
        _ if timed_out => (HookStatus::TimedOut, None),
        Ok(exit) if exit.success() => (HookStatus::Passed, exit.code()),
        Ok(exit) => (HookStatus::Failed, exit.code()),
        Err(io) => (HookStatus::Error(format!("waiting on hook: {io}")), None),
    };
    HookRun {
        id: hook.id.clone(),
        status,
        exit_code,
        stdout,
        stderr,
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
            Err(io) => return (Err(io), false),
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

/// Reads one stream to end-of-file, keeping the first `capture_bytes` and
/// counting all. The loop is bounded by the stream itself: it ends when the
/// hook's process exits or closes the pipe, which `wait_bounded` forces by
/// the configured timeout.
fn drain(mut stream: impl Read, capture_bytes: usize) -> CapturedStream {
    let mut kept: Vec<u8> = Vec::with_capacity(capture_bytes.min(STREAM_READ_BYTES));
    let mut total_bytes: u64 = 0;
    let mut buffer = [0_u8; STREAM_READ_BYTES];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read_bytes) => {
                total_bytes += read_bytes as u64;
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

    fn hook(argv: &[&str]) -> CommandHook {
        CommandHook {
            r#type: HookType::Command,
            id: "probe".to_owned(),
            kind: HookKind::Other,
            argv: argv.iter().map(|argument| (*argument).to_owned()).collect(),
            changed_paths: ChangedPaths::None,
            working_directory: ProjectPath(String::new()),
            environment: BTreeMap::new(),
            timeout_ms: 10_000,
            output_limit_bytes: 4_096,
            guarantees: Vec::new(),
            determinism: Determinism::Deterministic,
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
        let mut echo = hook(&["echo", "hello"]);
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
        let runs = run_hooks(&[hook(&["false"])], directory.path(), &[]);
        assert_eq!(runs[0].status, HookStatus::Failed);
        assert_eq!(runs[0].exit_code, Some(1));
    }

    #[test]
    fn test_timeout_kills_the_hook_without_waiting_it_out() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut slow = hook(&["sleep", "10"]);
        slow.timeout_ms = 200;
        let started = Instant::now();
        let runs = run_hooks(&[slow], directory.path(), &[]);
        assert_eq!(runs[0].status, HookStatus::TimedOut);
        assert_eq!(runs[0].exit_code, None);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the kill must not wait out the sleep: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn test_output_is_capped_and_full_size_reported() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut noisy = hook(&["seq", "1", "5000"]);
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
        let mut in_sub = hook(&["pwd"]);
        in_sub.working_directory = ProjectPath("sub".to_owned());
        let mut with_environment = hook(&["printenv", "RIFT_HOOK_PROBE"]);
        with_environment
            .environment
            .insert("RIFT_HOOK_PROBE".to_owned(), "42".to_owned());
        let runs = run_hooks(&[in_sub, with_environment], directory.path(), &[]);
        assert!(
            runs[0].stdout.text.trim_end().ends_with("/sub"),
            "{}",
            runs[0].stdout.text
        );
        assert_eq!(runs[1].stdout.text, "42\n");
    }

    #[test]
    fn test_missing_program_is_an_error_not_a_panic() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runs = run_hooks(
            &[hook(&["rift-test-binary-that-does-not-exist"])],
            directory.path(),
            &[],
        );
        assert!(
            matches!(&runs[0].status, HookStatus::Error(message) if message.contains("launch")),
            "{:?}",
            runs[0].status
        );
    }

    #[test]
    fn test_absolute_program_is_refused_before_spawning() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runs = run_hooks(&[hook(&["/bin/echo", "hi"])], directory.path(), &[]);
        assert!(
            matches!(&runs[0].status, HookStatus::Error(message) if message.contains("absolute")),
            "{:?}",
            runs[0].status
        );
    }

    #[test]
    fn test_empty_argv_is_an_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        let runs = run_hooks(&[hook(&[])], directory.path(), &[]);
        assert!(matches!(&runs[0].status, HookStatus::Error(message) if message.contains("argv")));
    }

    #[test]
    fn test_hooks_run_in_list_order() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut first = hook(&["echo", "first"]);
        first.id = "first".to_owned();
        let mut second = hook(&["echo", "second"]);
        second.id = "second".to_owned();
        let runs = run_hooks(&[first, second], directory.path(), &[]);
        assert_eq!(runs[0].id, "first");
        assert_eq!(runs[1].id, "second");
    }
}
