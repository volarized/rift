//! Runs one child process to completion under a wall-clock bound and a stream ceiling.
//!
//! [`run_bounded`] starts the caller's command with its standard input closed
//! and both output streams piped, drains each stream on its own thread keeping
//! a caller-sized prefix, and kills the child at `timeout`. The hook runner and
//! the dependency inspector both run their children through it.

use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rift_core::{CapturedStream, STREAM_READ_BYTES, STREAM_TOTAL_BYTES_MAX};

/// How long the runner sleeps between checks on a running child. The wait
/// loop wakes at most `timeout / PROCESS_POLL_INTERVAL + 1` times.
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// What one bounded child run produced.
#[derive(Debug)]
pub(crate) struct BoundedRun {
    /// The exit status, or the error that left the child unobservable.
    pub(crate) exit: std::io::Result<ExitStatus>,
    /// Whether the child overstayed `timeout` and was killed.
    pub(crate) timed_out: bool,
    /// Standard output, bounded by `capture_bytes`.
    pub(crate) stdout: CapturedStream,
    /// Standard error, bounded by `capture_bytes`.
    pub(crate) stderr: CapturedStream,
}

/// Spawns `command` with piped streams and null stdin, drains both, kills it at `timeout`.
///
/// The caller sets the program, arguments, working directory, and environment
/// overlay; the runner sets the three standard streams. The child inherits the
/// server's environment, `std::process::Command`'s default, and the `envs` the
/// caller laid on the command win over the inherited values. Each stream keeps
/// its first `capture_bytes` and counts the rest up to [`STREAM_TOTAL_BYTES_MAX`].
///
/// # Errors
///
/// Returns the spawn error when the child could not be started.
pub(crate) fn run_bounded(
    command: &mut Command,
    timeout: Duration,
    capture_bytes: usize,
) -> std::io::Result<BoundedRun> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout_drain = drain_thread(child.stdout.take(), capture_bytes);
    let stderr_drain = drain_thread(child.stderr.take(), capture_bytes);
    let (exit, timed_out) = wait_bounded(&mut child, timeout);
    Ok(BoundedRun {
        exit,
        timed_out,
        stdout: join_drain(stdout_drain),
        stderr: join_drain(stderr_drain),
    })
}

/// Waits for the child within `timeout`, then kills it. The poll loop wakes
/// at most `timeout / PROCESS_POLL_INTERVAL + 1` times before the deadline
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
        std::thread::sleep(remaining.min(PROCESS_POLL_INTERVAL));
    }
}

/// Starts one reader thread over a child stream. Null where the pipe was
/// not handed over, which piped children never do.
fn drain_thread(
    stream: Option<impl Read + Send + 'static>,
    capture_bytes: usize,
) -> Option<std::thread::JoinHandle<CapturedStream>> {
    let stream = stream?;
    Some(std::thread::spawn(move || drain(stream, capture_bytes)))
}

/// The thread's captured stream. A reader that panicked reports an empty
/// capture rather than tearing down the run.
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
    fn test_run_bounded_kills_the_child_at_timeout() {
        let mut command = Command::new("sleep");
        command.arg("30");
        let started = Instant::now();
        let run =
            run_bounded(&mut command, Duration::from_millis(200), 64).expect("sleep launches");
        let elapsed = started.elapsed();
        assert!(run.timed_out, "{run:?}");
        assert!(run.exit.is_ok(), "the killed child is reaped: {run:?}");
        assert!(
            elapsed < Duration::from_secs(2),
            "the kill must not wait out the sleep: {elapsed:?}"
        );
    }

    #[test]
    fn test_run_bounded_reports_the_exit_and_both_streams() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf out; printf err >&2; exit 3"]);
        let run = run_bounded(&mut command, Duration::from_secs(10), 64).expect("sh launches");
        assert!(!run.timed_out);
        let exit = run.exit.expect("sh is observed to its end");
        assert_eq!(exit.code(), Some(3));
        assert_eq!(run.stdout.text, "out");
        assert_eq!(run.stderr.text, "err");
        assert!(!run.stdout.truncated);
    }

    #[test]
    fn test_run_bounded_reports_a_missing_program_as_the_spawn_error() {
        let mut command = Command::new("rift-test-binary-that-does-not-exist");
        let error = run_bounded(&mut command, Duration::from_secs(10), 64)
            .expect_err("a missing program cannot spawn");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn test_run_bounded_inherits_the_server_environment() {
        let mut command = Command::new("printenv");
        command.arg("PATH");
        let run =
            run_bounded(&mut command, Duration::from_secs(10), 8 << 10).expect("printenv launches");
        let expected = std::env::var("PATH").expect("the test process has a PATH");
        assert_eq!(run.stdout.text.trim_end_matches('\n'), expected);
    }

    #[test]
    fn test_run_bounded_overlay_wins_over_the_inherited_value() {
        let mut command = Command::new("printenv");
        command.arg("HOME").env("HOME", "/rift/overlay");
        let run =
            run_bounded(&mut command, Duration::from_secs(10), 64).expect("printenv launches");
        assert_eq!(run.stdout.text, "/rift/overlay\n");
    }
}
