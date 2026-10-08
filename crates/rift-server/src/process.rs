//! Runs one child process to completion under a wall-clock bound and a stream ceiling.
//!
//! [`run_bounded`] starts the caller's command with its standard input closed
//! and both output streams piped, drains each stream on its own thread keeping
//! a caller-sized prefix, and kills the child at `timeout` or once the caller's
//! `cancelled` answers true. The dependency inspector runs its children through it.
//!
//! On Windows the child starts inside a Job object of its own, created with the child
//! through `windows-spawn`, so every process the child starts belongs to the Job and
//! ends with it. On Unix only the child itself is killed; a process it started that
//! still holds the output pipes is left running, and the run stops waiting on it.

use std::io::Read;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(not(windows))]
pub(crate) use std::process::{Child, Command, Stdio};
#[cfg(windows)]
pub(crate) use windows_spawn::{Child, Command, Stdio};

use rift_core::{CapturedStream, STREAM_READ_BYTES, STREAM_TOTAL_BYTES_MAX};

/// How long the runner sleeps between checks on a running child. The wait
/// loop wakes at most `timeout / PROCESS_POLL_INTERVAL + 1` times, and reads the
/// caller's cancellation once per wake.
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// How long a run whose child was killed, at `timeout` or at the caller's cancellation,
/// waits for both output pipes to reach end-of-file once the child is reaped. A process
/// the child started can hold the pipes open past the kill; the run returns what was
/// read by then.
const KILLED_PIPE_WAIT: Duration = Duration::from_millis(200);

/// Stream reader threads alive at once across every run of this process. Each run takes
/// two before it spawns; a reader a run stopped waiting on keeps its slot until the last
/// process holding its pipe closes it.
const STREAM_READERS_MAX: usize = 16;

/// Stream reader threads alive now, at most [`STREAM_READERS_MAX`].
static STREAM_READERS: AtomicUsize = AtomicUsize::new(0);

/// How one bounded run's wait ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunEnding {
    /// The child exited on its own, or could not be observed and was killed and reaped.
    Exited,
    /// The child overstayed `timeout` and was killed, or exited while its output pipes
    /// stayed open past `timeout`.
    TimedOut,
    /// The caller cancelled the run while the child ran, and the child was killed.
    Cancelled,
}

/// What one bounded child run produced.
#[derive(Debug)]
pub(crate) struct BoundedRun {
    /// The operating system's process identifier of the child.
    pub(crate) pid: u32,
    /// The exit status, or the error that left the child unobservable.
    pub(crate) exit: std::io::Result<ExitStatus>,
    /// How the wait ended: on its own, at `timeout`, or at the caller's cancellation.
    pub(crate) ending: RunEnding,
    /// Standard output, bounded by `capture_bytes`.
    pub(crate) stdout: CapturedStream,
    /// Standard error, bounded by `capture_bytes`.
    pub(crate) stderr: CapturedStream,
}

/// Spawns `command` with piped streams and null stdin, drains both, kills it at `timeout`
/// or once `cancelled` answers true.
///
/// The caller sets the program, arguments, working directory, and environment
/// overlay; the runner sets the three standard streams. The child inherits the
/// server's environment, the default of both `std::process::Command` and
/// `windows_spawn::Command`, and the `envs` the caller laid on the command win over the
/// inherited values. On Windows the child inherits only the three standard streams
/// (`windows-spawn` passes an explicit handle list). Each stream keeps its first
/// `capture_bytes` and counts the rest up to [`STREAM_TOTAL_BYTES_MAX`].
///
/// A cancellation reaches the child within one [`PROCESS_POLL_INTERVAL`] (25 ms) plus
/// one `try_wait`: the wait loop reads `cancelled` once per wake, then kills the child by
/// its own handle (`SIGKILL` on Unix, `TerminateProcess` on Windows) and reaps it. On
/// Windows the run then terminates the child's Job, which ends every process the child
/// started. The run never waits on a pipe a process outside its reach still holds:
///
/// - killed at `timeout` or at cancellation, it waits at most [`KILLED_PIPE_WAIT`] after
///   the reap for both pipes to close;
/// - exited on its own, it waits for both pipes until `timeout` from the spawn, or
///   [`KILLED_PIPE_WAIT`] after the exit when that is later, reading `cancelled` once
///   per [`PROCESS_POLL_INTERVAL`]. Pipes still open then end the run `TimedOut`, and a
///   cancellation read meanwhile ends it `Cancelled`.
///
/// So every run returns within `timeout + PROCESS_POLL_INTERVAL + KILLED_PIPE_WAIT` plus
/// the kill and the reap, and a cancelled run within `PROCESS_POLL_INTERVAL +
/// KILLED_PIPE_WAIT` of the cancellation plus the kill and the reap. A reader the run
/// stopped waiting on reads on until its pipe closes, then exits; at most
/// [`STREAM_READERS_MAX`] readers are alive at once, each holding one
/// [`STREAM_READ_BYTES`] buffer and at most `capture_bytes` kept.
///
/// # Errors
///
/// Returns the spawn error when the child could not be started, and refuses to spawn
/// while [`STREAM_READERS_MAX`] readers of earlier runs are still alive.
pub(crate) fn run_bounded(
    command: &mut Command,
    timeout: Duration,
    capture_bytes: usize,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<BoundedRun> {
    let [stdout_slot, stderr_slot] = reserve_readers(&STREAM_READERS)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let deadline = Instant::now() + timeout;
    let (mut child, tree) = ProcessTree::spawn(command)?;
    let pid = child.id();
    rift_tracing::info!(
        component = "process",
        pid,
        stdin = "null",
        stdout = "piped",
        stderr = "piped",
        "child spawned"
    );
    let mut readers = [
        StreamReader::start(child.stdout.take(), capture_bytes, stdout_slot),
        StreamReader::start(child.stderr.take(), capture_bytes, stderr_slot),
    ];
    let (exit, mut ending) = rift_tracing::traced!(
        component = "process",
        operation = "process.wait",
        open = true,
        pid = pid,
        stdin = "null",
        stdout = "piped",
        stderr = "piped",
        outcome = rift_tracing::empty!(),
        {
            let waited = wait_bounded(&mut child, deadline, cancelled);
            let outcome = match waited.1 {
                RunEnding::Exited if waited.0.is_ok() => "ok",
                RunEnding::Exited => "refused",
                RunEnding::TimedOut => "timeout",
                RunEnding::Cancelled => "cancelled",
            };
            rift_tracing::Span::current().record("outcome", outcome);
            waited
        }
    );
    let exit_code = exit.as_ref().ok().and_then(ExitStatus::code);
    rift_tracing::info!(
        component = "process",
        pid,
        exit_code,
        stdin = "null",
        stdout = "piped",
        stderr = "piped",
        "child exited"
    );
    tree.end();
    let reaped = Instant::now();
    let collected = match ending {
        RunEnding::Exited => collect_streams(
            &mut readers,
            deadline.max(reaped + KILLED_PIPE_WAIT),
            Some(cancelled),
        ),
        RunEnding::TimedOut | RunEnding::Cancelled => {
            collect_streams(&mut readers, reaped + KILLED_PIPE_WAIT, None)
        }
    };
    match (ending, collected) {
        (RunEnding::Exited, PipesEnding::Cancelled) => ending = RunEnding::Cancelled,
        (RunEnding::Exited, PipesEnding::Expired) => ending = RunEnding::TimedOut,
        _ => {}
    }
    let [stdout, stderr] = readers.map(StreamReader::into_captured);
    Ok(BoundedRun {
        pid,
        exit,
        ending,
        stdout,
        stderr,
    })
}

/// The processes a run's child started, where the platform lets the run end them.
struct ProcessTree {
    /// The Job the child was created in, with kill-on-close set: every process the child
    /// starts joins it, and dropping it ends them all.
    #[cfg(windows)]
    job: windows_spawn::Job,
}

impl ProcessTree {
    /// Spawns `command` as a plain child; Unix keeps no handle on its descendants.
    #[cfg(not(windows))]
    fn spawn(command: &mut Command) -> std::io::Result<(Child, Self)> {
        Ok((command.spawn()?, Self {}))
    }

    /// Spawns `command` inside a new Job object. `windows-spawn` names the Job in the
    /// child's `PROC_THREAD_ATTRIBUTE_JOB_LIST`, so the child belongs to it from its
    /// creation and nothing it starts can escape it by racing an assignment. Under a
    /// Job the server already runs in, the new Job nests inside it.
    #[cfg(windows)]
    fn spawn(command: &mut Command) -> std::io::Result<(Child, Self)> {
        let job = windows_spawn::Job::create()?;
        job.set_kill_on_close(true)?;
        let child = command.spawn_with(windows_spawn::SpawnOptions::new().job(&job))?;
        Ok((child, Self { job }))
    }

    /// Ends every process still in the tree once the child was reaped. Nothing on Unix.
    #[cfg_attr(
        not(windows),
        expect(clippy::unused_self, reason = "Unix keeps no handle on the tree")
    )]
    fn end(&self) {
        #[cfg(windows)]
        {
            // A Job whose processes all exited terminates nothing; dropping it then
            // closes the last handle, which ends any process the call left.
            let _ = self.job.terminate(1);
        }
    }
}

/// Waits for the child until `deadline`, then kills it. The poll loop wakes
/// at most `timeout / PROCESS_POLL_INTERVAL + 1` times before the deadline
/// forces the kill, and kills the child at the first wake that finds `cancelled`
/// true, so a cancellation waits at most one [`PROCESS_POLL_INTERVAL`] for its kill.
fn wait_bounded(
    child: &mut Child,
    deadline: Instant,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> (std::io::Result<ExitStatus>, RunEnding) {
    loop {
        match child.try_wait() {
            Ok(Some(exit)) => return (Ok(exit), RunEnding::Exited),
            Ok(None) => {}
            Err(io) => {
                // The child's state is unknown, and an unobservable child must
                // not outlive its bound: kill it before reporting the failure.
                let _ = child.kill();
                let _ = child.wait();
                return (Err(io), RunEnding::Exited);
            }
        }
        if cancelled() {
            let _ = child.kill();
            return (child.wait(), RunEnding::Cancelled);
        }
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
        else {
            let _ = child.kill();
            return (child.wait(), RunEnding::TimedOut);
        };
        std::thread::sleep(remaining.min(PROCESS_POLL_INTERVAL));
    }
}

/// One count in [`STREAM_READERS`], released when dropped.
struct ReaderSlot(&'static AtomicUsize);

impl Drop for ReaderSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Takes the two reader slots one run needs, or refuses while `readers` has fewer than
/// two left of [`STREAM_READERS_MAX`].
fn reserve_readers(readers: &'static AtomicUsize) -> std::io::Result<[ReaderSlot; 2]> {
    readers
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |alive| {
            (alive + 2 <= STREAM_READERS_MAX).then_some(alive + 2)
        })
        .map_err(|alive| {
            std::io::Error::other(format!(
                "{alive} output readers of earlier runs still wait on processes those runs \
                 left running; at most {STREAM_READERS_MAX} run at once"
            ))
        })?;
    Ok([ReaderSlot(readers), ReaderSlot(readers)])
}

/// One stream's reader thread and, once it reported, its capture.
struct StreamReader {
    /// Receives the capture once the thread reached end-of-file or the drain ceiling.
    /// Disconnected without a capture when the thread panicked.
    receiver: Option<mpsc::Receiver<CapturedStream>>,
    /// The capture, once received.
    captured: Option<CapturedStream>,
}

impl StreamReader {
    /// Starts one reader thread over a child stream, holding `slot` until it exits. An
    /// absent stream, which piped children never have, reports an empty capture.
    fn start(
        stream: Option<impl Read + Send + 'static>,
        capture_bytes: usize,
        slot: ReaderSlot,
    ) -> Self {
        let Some(stream) = stream else {
            return Self {
                receiver: None,
                captured: Some(CapturedStream::default()),
            };
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _slot = slot;
            // A run that stopped waiting dropped the receiver; the capture is discarded.
            let _ = sender.send(drain(stream, capture_bytes));
        });
        Self {
            receiver: Some(receiver),
            captured: None,
        }
    }

    /// Takes the capture if the thread finished, without blocking. A thread that
    /// panicked counts as finished with an empty capture.
    fn poll(&mut self) -> bool {
        if self.captured.is_some() {
            return true;
        }
        let Some(receiver) = &self.receiver else {
            return true;
        };
        match receiver.try_recv() {
            Ok(captured) => self.captured = Some(captured),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.captured = Some(CapturedStream::default());
            }
            Err(mpsc::TryRecvError::Empty) => return false,
        }
        true
    }

    /// Blocks at most `wait` for the thread's capture.
    fn wait(&mut self, wait: Duration) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        match receiver.recv_timeout(wait) {
            Ok(captured) => self.captured = Some(captured),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.captured = Some(CapturedStream::default());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }

    /// The capture, or an empty one when the run stopped waiting before it arrived.
    /// Dropping the receiver detaches the thread, which exits at its pipe's close.
    fn into_captured(self) -> CapturedStream {
        self.captured.unwrap_or_default()
    }
}

/// How waiting on both output pipes ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PipesEnding {
    /// Both readers reported.
    Closed,
    /// The caller cancelled before both reported.
    Cancelled,
    /// `until` passed before both reported.
    Expired,
}

/// Waits for both readers until `until`, reading `cancelled`, when given, once per wake.
/// Each wake either receives one reader's capture, at most two times, or waits out one
/// [`PROCESS_POLL_INTERVAL`], so the loop wakes at most
/// `(until - now) / PROCESS_POLL_INTERVAL + 3` times.
fn collect_streams(
    readers: &mut [StreamReader; 2],
    until: Instant,
    cancelled: Option<&(dyn Fn() -> bool + Sync)>,
) -> PipesEnding {
    loop {
        let [stdout, stderr] = readers;
        let stdout_closed = stdout.poll();
        let stderr_closed = stderr.poll();
        let open = match (stdout_closed, stderr_closed) {
            (true, true) => return PipesEnding::Closed,
            (false, _) => stdout,
            (true, false) => stderr,
        };
        if cancelled.is_some_and(|cancelled| cancelled()) {
            return PipesEnding::Cancelled;
        }
        let Some(remaining) = until
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
        else {
            return PipesEnding::Expired;
        };
        open.wait(remaining.min(PROCESS_POLL_INTERVAL));
    }
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

/// A child process built from this test binary, for runs every platform can make.
///
/// [`fixture_command`] re-runs the current test executable on the ignored
/// [`process_fixture`] test, which reads [`FIXTURE_MODE`] and acts it out. The
/// executable is a libtest harness, so its standard output also carries libtest's own
/// lines; the fixture's standard error carries only what the mode writes.
#[cfg(test)]
pub(crate) mod fixture {
    use std::path::Path;
    use std::time::Duration;

    use super::Command;

    /// The variable naming what [`process_fixture`] does.
    const FIXTURE_MODE: &str = "RIFT_PROCESS_FIXTURE";
    /// The variable naming the file a holding fixture writes its child's pid to.
    const FIXTURE_PID_FILE: &str = "RIFT_PROCESS_FIXTURE_PID_FILE";
    /// The variable a `print-variable` fixture prints the value of.
    const FIXTURE_VARIABLE: &str = "RIFT_PROCESS_FIXTURE_VARIABLE";
    /// The test the fixture command runs.
    const FIXTURE_TEST: &str = "process::fixture::process_fixture";
    /// How long a sleeping fixture sleeps; every run that waits it out fails its bound.
    pub(crate) const FIXTURE_SLEEP: Duration = Duration::from_secs(30);
    /// What a `print` fixture writes to standard output.
    pub(crate) const PRINTED_STDOUT: &str = "rift-fixture-stdout";
    /// What a `print` fixture writes to standard error.
    pub(crate) const PRINTED_STDERR: &str = "rift-fixture-stderr";
    /// The exit code of a `print` fixture.
    pub(crate) const PRINTED_EXIT_CODE: i32 = 3;

    /// What one fixture process does.
    #[derive(Clone, Copy, Debug)]
    pub(crate) enum FixtureMode {
        /// Sleeps [`FIXTURE_SLEEP`].
        Sleep,
        /// Starts a `Sleep` fixture that inherits its standard streams, writes that
        /// child's pid to the pid file, then sleeps [`FIXTURE_SLEEP`].
        Hold,
        /// Writes [`PRINTED_STDOUT`] and [`PRINTED_STDERR`], exits [`PRINTED_EXIT_CODE`].
        Print,
        /// Writes the value of the variable [`FIXTURE_VARIABLE`] names to standard error.
        PrintVariable,
    }

    impl FixtureMode {
        const fn name(self) -> &'static str {
            match self {
                Self::Sleep => "sleep",
                Self::Hold => "hold",
                Self::Print => "print",
                Self::PrintVariable => "print-variable",
            }
        }
    }

    /// The command running one fixture process in `mode`.
    pub(crate) fn fixture_command(mode: FixtureMode) -> Command {
        let program = std::env::current_exe().expect("the test binary has a path");
        let mut command = Command::new(program);
        command
            .args(["--exact", FIXTURE_TEST, "--ignored", "--nocapture"])
            .env(FIXTURE_MODE, mode.name());
        command
    }

    /// A `Hold` fixture that writes its child's pid to `pid_file`.
    pub(crate) fn holding_command(pid_file: &Path) -> Command {
        let mut command = fixture_command(FixtureMode::Hold);
        command.env(FIXTURE_PID_FILE, pid_file);
        command
    }

    /// A `PrintVariable` fixture printing `variable`.
    pub(crate) fn variable_command(variable: &str) -> Command {
        let mut command = fixture_command(FixtureMode::PrintVariable);
        command.env(FIXTURE_VARIABLE, variable);
        command
    }

    /// The pid a `Hold` fixture wrote, once the file is complete.
    pub(crate) fn held_pid(pid_file: &Path) -> Option<u32> {
        std::fs::read_to_string(pid_file).ok()?.trim().parse().ok()
    }

    /// Whether the process `pid` is still running, asked of the platform's own tool.
    pub(crate) fn is_running(pid: u32) -> bool {
        #[cfg(unix)]
        let output = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output();
        #[cfg(windows)]
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output();
        let output = output.expect("the process tool runs");
        if cfg!(windows) {
            String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
        } else {
            output.status.success()
        }
    }

    /// Kills the process `pid` the test itself started, by that pid alone.
    pub(crate) fn kill_pid(pid: u32) {
        #[cfg(unix)]
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .output();
        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output();
    }

    /// Acts out [`FIXTURE_MODE`]; does nothing when the variable is absent, so a run
    /// that selects ignored tests passes it.
    #[test]
    #[ignore = "runs only as a child of the process runner tests"]
    fn process_fixture() {
        use std::io::Write as _;

        let Ok(mode) = std::env::var(FIXTURE_MODE) else {
            return;
        };
        match mode.as_str() {
            "sleep" => std::thread::sleep(FIXTURE_SLEEP),
            "hold" => {
                let pid_file = std::env::var_os(FIXTURE_PID_FILE).expect("a pid file");
                let mut child = std::process::Command::new(
                    std::env::current_exe().expect("the test binary has a path"),
                )
                .args(["--exact", FIXTURE_TEST, "--ignored", "--nocapture"])
                .env(FIXTURE_MODE, FixtureMode::Sleep.name())
                .env_remove(FIXTURE_PID_FILE)
                .spawn()
                .expect("the held child spawns");
                // Written beside, then renamed, so a reader never sees a partial pid.
                let partial = Path::new(&pid_file).with_extension("partial");
                std::fs::write(&partial, child.id().to_string()).expect("write the pid");
                std::fs::rename(&partial, &pid_file).expect("publish the pid");
                std::thread::sleep(FIXTURE_SLEEP);
                let _ = child.wait();
            }
            "print" => {
                print!("{PRINTED_STDOUT}");
                eprint!("{PRINTED_STDERR}");
                let _ = std::io::stdout().flush();
                let _ = std::io::stderr().flush();
                std::process::exit(PRINTED_EXIT_CODE);
            }
            "print-variable" => {
                let variable = std::env::var(FIXTURE_VARIABLE).expect("a variable name");
                let value = std::env::var(variable).unwrap_or_default();
                eprint!("{value}");
            }
            other => panic!("unknown fixture mode {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::OnceLock;

    use super::fixture::{
        FIXTURE_SLEEP, FixtureMode, PRINTED_EXIT_CODE, PRINTED_STDERR, PRINTED_STDOUT,
        fixture_command, held_pid, holding_command, is_running, kill_pid, variable_command,
    };
    use super::*;

    /// Slack over a run's derived bound for scheduling and the fixture's own start; far
    /// below [`FIXTURE_SLEEP`], so a run that waited out a sleeping process fails.
    const BOUND_SLACK: Duration = Duration::from_secs(5);

    /// How long a test polls for a process the run's Job ended to leave the system.
    const PROCESS_GONE_WAIT: Duration = Duration::from_secs(10);

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

    /// A stream whose reader thread panics, for proving the receive fallback.
    struct PanickingStream;

    impl Read for PanickingStream {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("reader died mid-stream");
        }
    }

    #[test]
    fn test_panicked_reader_reports_an_empty_capture() {
        static READERS: AtomicUsize = AtomicUsize::new(0);
        let [panicking, absent] = reserve_readers(&READERS).expect("slots are free");
        let mut readers = [
            StreamReader::start(Some(PanickingStream), 16, panicking),
            StreamReader::start(None::<PanickingStream>, 16, absent),
        ];
        let until = Instant::now() + Duration::from_secs(10);
        assert_eq!(
            collect_streams(&mut readers, until, None),
            PipesEnding::Closed
        );
        let [captured, absent] = readers.map(StreamReader::into_captured);
        assert_eq!(captured, CapturedStream::default());
        assert_eq!(absent, CapturedStream::default());
    }

    /// Reader slots are released as their holders drop, and a run is refused while
    /// fewer than two are left.
    #[test]
    fn test_reader_slots_refuse_a_run_past_the_ceiling() {
        static READERS: AtomicUsize = AtomicUsize::new(0);
        let held: Vec<[ReaderSlot; 2]> = (0..STREAM_READERS_MAX / 2)
            .map(|_| reserve_readers(&READERS).expect("under the ceiling"))
            .collect();
        assert_eq!(READERS.load(Ordering::Acquire), STREAM_READERS_MAX);
        let refusal = reserve_readers(&READERS)
            .err()
            .expect("the ceiling refuses a run");
        assert!(refusal.to_string().contains("output readers"), "{refusal}");
        drop(held);
        assert_eq!(READERS.load(Ordering::Acquire), 0);
        assert!(reserve_readers(&READERS).is_ok());
    }

    /// A cancellation that answers true from its first read after `trigger` holds, and
    /// remembers when it first did.
    struct CancelAfter<'trigger> {
        trigger: &'trigger (dyn Fn() -> bool + Sync),
        at: OnceLock<Instant>,
    }

    impl<'trigger> CancelAfter<'trigger> {
        fn new(trigger: &'trigger (dyn Fn() -> bool + Sync)) -> Self {
            Self {
                trigger,
                at: OnceLock::new(),
            }
        }

        fn read(&self) -> bool {
            if self.at.get().is_some() || (self.trigger)() {
                self.at.get_or_init(Instant::now);
                return true;
            }
            false
        }

        fn cancelled_at(&self) -> Instant {
            *self.at.get().expect("the run read the cancellation")
        }
    }

    /// A child killed by its own handle: `SIGKILL` on Unix, `TerminateProcess`
    /// with code 1 on Windows.
    fn assert_killed(run: &BoundedRun) {
        let exit = run.exit.as_ref().expect("the killed child is reaped");
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            assert_eq!(exit.signal(), Some(9), "killed by SIGKILL: {exit:?}");
        }
        #[cfg(windows)]
        assert_eq!(exit.code(), Some(1), "killed by TerminateProcess: {exit:?}");
    }

    /// Polls until `pid` left the system or [`PROCESS_GONE_WAIT`] passed.
    fn left_the_system(pid: u32) -> bool {
        let until = Instant::now() + PROCESS_GONE_WAIT;
        while Instant::now() < until {
            if !is_running(pid) {
                return true;
            }
            std::thread::sleep(PROCESS_POLL_INTERVAL);
        }
        false
    }

    /// After a run whose child held a grandchild: on Windows the run's Job ended the
    /// grandchild; on Unix the run leaves it, and the test kills its own grandchild by pid.
    fn assert_grandchild_ended_where_the_job_reaches(pid_file: &Path) {
        let pid = held_pid(pid_file).expect("the holding fixture wrote its child's pid");
        if cfg!(windows) {
            let gone = left_the_system(pid);
            if !gone {
                kill_pid(pid);
            }
            assert!(gone, "the Job ends the grandchild {pid}");
        } else {
            kill_pid(pid);
        }
    }

    #[test]
    fn test_run_bounded_kills_the_child_at_timeout() {
        let timeout = Duration::from_millis(200);
        let started = Instant::now();
        let run = run_bounded(
            &mut fixture_command(FixtureMode::Sleep),
            timeout,
            64,
            &|| false,
        )
        .expect("the fixture launches");
        let elapsed = started.elapsed();
        assert_eq!(run.ending, RunEnding::TimedOut, "{run:?}");
        assert_killed(&run);
        assert!(
            elapsed < timeout + KILLED_PIPE_WAIT + BOUND_SLACK,
            "the kill must not wait out the sleep: {elapsed:?}"
        );
    }

    /// A cancellation the wait reads while the child runs kills the child by its own
    /// handle and reaps it within one poll interval and the kill, far inside its timeout.
    #[test]
    fn test_run_bounded_kills_and_reaps_the_child_once_cancelled() {
        /// The wait reads cancellation once per wake; the third read answers true, so the
        /// child has run through two poll intervals first.
        const READS_BEFORE_CANCEL: usize = 2;
        let reads = AtomicUsize::new(0);
        let cancelled = || reads.fetch_add(1, Ordering::Relaxed) >= READS_BEFORE_CANCEL;
        let started = Instant::now();
        let run = run_bounded(
            &mut fixture_command(FixtureMode::Sleep),
            FIXTURE_SLEEP,
            64,
            &cancelled,
        )
        .expect("the fixture launches");
        let elapsed = started.elapsed();

        assert_eq!(run.ending, RunEnding::Cancelled, "{run:?}");
        assert_killed(&run);
        assert_eq!(reads.load(Ordering::Relaxed), READS_BEFORE_CANCEL + 1);
        assert!(
            elapsed < PROCESS_POLL_INTERVAL * 3 + KILLED_PIPE_WAIT + BOUND_SLACK,
            "the kill must not wait out the sleep: {elapsed:?}"
        );
    }

    /// A cancellation while a grandchild holds both output pipes: the run kills its child,
    /// stops waiting on the pipes after [`KILLED_PIPE_WAIT`], and returns `Cancelled`.
    #[test]
    fn test_run_bounded_returns_once_cancelled_while_a_grandchild_holds_the_pipes() {
        let directory = tempfile::tempdir().expect("tempdir");
        let pid_file = directory.path().join("held.pid");
        let trigger = || pid_file.exists();
        let cancel = CancelAfter::new(&trigger);
        let run = run_bounded(&mut holding_command(&pid_file), FIXTURE_SLEEP, 64, &|| {
            cancel.read()
        })
        .expect("the fixture launches");
        let after_cancel = cancel.cancelled_at().elapsed();
        assert_grandchild_ended_where_the_job_reaches(&pid_file);

        assert_eq!(run.ending, RunEnding::Cancelled, "{run:?}");
        assert_killed(&run);
        assert!(
            after_cancel < PROCESS_POLL_INTERVAL + KILLED_PIPE_WAIT + BOUND_SLACK,
            "the run must not wait on the held pipes: {after_cancel:?}"
        );
    }

    /// A timeout while a grandchild holds both output pipes: same bound after the kill.
    #[test]
    fn test_run_bounded_returns_at_timeout_while_a_grandchild_holds_the_pipes() {
        let timeout = Duration::from_secs(3);
        let directory = tempfile::tempdir().expect("tempdir");
        let pid_file = directory.path().join("held.pid");
        let started = Instant::now();
        let run = run_bounded(&mut holding_command(&pid_file), timeout, 64, &|| false)
            .expect("the fixture launches");
        let elapsed = started.elapsed();
        assert_grandchild_ended_where_the_job_reaches(&pid_file);

        assert_eq!(run.ending, RunEnding::TimedOut, "{run:?}");
        assert_killed(&run);
        assert!(
            elapsed < timeout + KILLED_PIPE_WAIT + BOUND_SLACK,
            "the run must not wait on the held pipes: {elapsed:?}"
        );
    }

    /// A run nobody cancels reads the cancellation and still ends on its own exit.
    #[test]
    fn test_run_bounded_ends_on_the_exit_while_never_cancelled() {
        let run = run_bounded(
            &mut variable_command("PATH"),
            FIXTURE_SLEEP,
            64 << 10,
            &|| false,
        )
        .expect("the fixture launches");
        assert_eq!(run.ending, RunEnding::Exited);
        assert_eq!(run.exit.expect("the fixture is observed").code(), Some(0));
    }

    #[test]
    fn test_run_bounded_reports_the_exit_and_both_streams() {
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("recorder");
        let run = run_bounded(
            &mut fixture_command(FixtureMode::Print),
            FIXTURE_SLEEP,
            8 << 10,
            &|| false,
        )
        .expect("the fixture launches");
        assert_eq!(run.ending, RunEnding::Exited);
        let exit = run.exit.expect("the fixture is observed to its end");
        assert_eq!(exit.code(), Some(PRINTED_EXIT_CODE));
        assert!(run.stdout.text.contains(PRINTED_STDOUT), "{:?}", run.stdout);
        assert_eq!(run.stderr.text, PRINTED_STDERR);
        assert!(!run.stdout.truncated);
        drop(recorder);
        let records = drain.queued_records();
        for message in ["child spawned", "process.wait", "child exited"] {
            let record = records
                .iter()
                .find(|record| record.message() == message)
                .expect("child lifecycle record");
            let fields: serde_json::Value =
                serde_json::from_str(record.fields()).expect("record fields");
            assert_eq!(fields["pid"], run.pid.to_string());
            assert_eq!(fields["stdin"], "null");
            assert_eq!(fields["stdout"], "piped");
            assert_eq!(fields["stderr"], "piped");
        }
    }

    #[test]
    fn test_run_bounded_reports_a_missing_program_as_the_spawn_error() {
        let mut command = Command::new("rift-test-binary-that-does-not-exist");
        let error = run_bounded(&mut command, Duration::from_secs(10), 64, &|| false)
            .expect_err("a missing program cannot spawn");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn test_run_bounded_inherits_the_server_environment() {
        let mut command = variable_command("PATH");
        let run = run_bounded(&mut command, FIXTURE_SLEEP, 64 << 10, &|| false)
            .expect("the fixture launches");
        let expected = std::env::var("PATH").expect("the test process has a PATH");
        assert_eq!(run.stderr.text, expected);
    }

    #[test]
    fn test_run_bounded_overlay_wins_over_the_inherited_value() {
        let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        assert_ne!(
            std::env::var(variable).ok().as_deref(),
            Some("rift-overlay")
        );
        let mut command = variable_command(variable);
        command.env(variable, "rift-overlay");
        let run =
            run_bounded(&mut command, FIXTURE_SLEEP, 64, &|| false).expect("the fixture launches");
        assert_eq!(run.stderr.text, "rift-overlay");
    }
}
