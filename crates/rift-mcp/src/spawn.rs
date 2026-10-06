//! Detached spawning of one workspace's `rift server` process.
//!
//! The CLI's `rift server start` and the stdio proxy start the workspace's
//! server the same way: this binary again, as `rift server start
//! --foreground`, fully detached from the caller's terminal and process
//! group. The poll constants for waiting on the spawned server's published
//! lock document live beside the spawn, so every caller shares one meaning
//! of "the server came up in time".

use std::fmt::Debug;
use std::fs::File;
use std::io::{self, Read, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod process;
use process::{Child, Command, Stdio, detached_command_for, spawn_detached};

use rift_core::constants::RIFT_STATE_DIRECTORY;
use rift_core::{CapturedStream, STREAM_READ_BYTES, STREAM_TOTAL_BYTES_MAX};

/// Bytes of a detached server's startup stderr kept verbatim; the rest is
/// only counted, the same split [`CapturedStream`] reports for captured streams.
const STARTUP_STDERR_CAPTURE_BYTES: usize = 8 << 10;

/// The file under `.rift`, beside `server.json`, that holds the standard
/// error of the server `rift server start` spawns. Each start truncates it.
pub const SERVER_STDERR_FILE_NAME: &str = "server.stderr";
/// Pause between presence probes while waiting on a workspace's server.
pub const PRESENCE_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Longest wait for a spawned server to publish its lock document.
///
/// A start poll runs `START_WAIT_MAX / PRESENCE_POLL_INTERVAL` = 300
/// bounded iterations.
pub const START_WAIT_MAX: Duration = Duration::from_secs(30);
/// Probe attempts one start waits: [`START_WAIT_MAX`] over the interval.
pub const START_POLL_ATTEMPT_COUNT: u32 = 300;
/// Most detached servers one start spawns.
///
/// A spawned server loses the start election whenever its claim meets any
/// lock on the election file, a probe's shared lock included: Windows
/// releases a closed handle's locks lazily, so a probe that already let go
/// can still hold one. A loss that leaves the election unheld is that case,
/// and the start spawns again. The count bounds the spawns when something
/// keeps such a lock for longer; the rest of the start window then passes
/// as a wait.
pub const START_SPAWN_COUNT_MAX: u32 = 4;

fn debug_start_spawn(stage: &str, spawn_count: u32) {
    if !debug_lingering_lock_test() {
        return;
    }
    let unix_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    eprintln!(
        "DEBUG server start {stage} unix_ns={unix_ns} pid={} spawn_count={spawn_count}",
        std::process::id(),
    );
}

fn debug_lingering_lock_test() -> bool {
    let test = "$a_start_lost_to_a_lingering_shared_lock_spawns_again";
    std::env::var_os("NEXTEST_ATTEMPT_ID")
        .is_some_and(|attempt| attempt.to_string_lossy().ends_with(test))
}

fn debug_start_child_stderr(stderr: &str) {
    if !debug_lingering_lock_test() {
        return;
    }
    for line in stderr.lines().filter(|line| line.contains("DEBUG")) {
        eprintln!("DEBUG captured child stderr {line}");
    }
}
/// Bytes at the end of a detached server's stderr file read to classify its
/// exit: the refusal a server exits on is the last thing it writes there.
const EXIT_STDERR_TAIL_BYTES: u64 = 8 << 10;

/// Builds the detached-server command every spawn shares: this binary
/// again, `server start --foreground`, run inside `root`, off this
/// process's terminal and process group. Stdin and stdout are always
/// null; the caller sets stderr's policy before spawning.
///
/// # Errors
///
/// Returns the underlying failure when this binary's own path cannot be
/// read.
fn detached_command(root: &Path) -> Result<Command, io::Error> {
    let program = std::env::current_exe()?;
    Ok(detached_command_for(
        program,
        ["server", "start", "--foreground"],
        root,
    ))
}

/// Spawns `rift server start --foreground` for `root`, fully detached,
/// with its stderr routed to the workspace's [`SERVER_STDERR_FILE_NAME`].
///
/// The child runs this same binary with `root` as its working directory and
/// inherits this process's environment - it serves the workspace the caller
/// addressed - with stdin and stdout null, stderr on the file this start
/// truncates, and its own process group, so it survives the caller's exit
/// and its terminal. The returned handle is this process's view of its own
/// child: the caller polls the published lock document for the server, and
/// asks the handle whether the child is still running when that document
/// does not appear. An exited child is reaped by that ask, or by the init
/// process once the handle is gone.
///
/// Losing the election race is not a spawn failure: a child that finds the
/// workspace already served exits on its own, and the caller's poll adopts
/// whoever won.
///
/// Used by `rift server start`. `rift mcp` has no file to point an agent
/// at and uses `spawn_detached_server_with_captured_stderr` instead.
///
/// # Errors
///
/// Returns the underlying failure when this binary's path cannot be read
/// or the process cannot be spawned; callers classify it for their own
/// surface. A stderr file that cannot be created is reported and the
/// child's stderr is discarded, so a full or read-only `.rift` never stops
/// a start.
pub fn spawn_detached_server(root: &Path) -> Result<SpawnedServer, io::Error> {
    spawn_with_stderr_file(detached_command(root)?, root)
}

/// Spawns `command` detached, with its stderr on the workspace's
/// [`SERVER_STDERR_FILE_NAME`] below `root`, or discarded when that file
/// cannot be created.
fn spawn_with_stderr_file(mut command: Command, root: &Path) -> Result<SpawnedServer, io::Error> {
    let destination = stderr_destination(root);
    let stderr = destination.is_some().then(|| stderr_file_path(root));
    command.stderr(destination.map_or_else(Stdio::null, Stdio::from));
    let child = spawn_detached(&mut command)?;
    record_spawn(child.id(), if stderr.is_some() { "file" } else { "null" });
    Ok(SpawnedServer { child, stderr })
}

/// Records one detached server this process spawned: its process identifier and where
/// each standard stream goes. The child inherits none of this process's streams: stdin
/// and stdout are null, and stderr is `file`, `piped`, or `null`.
fn record_spawn(pid: u32, stderr: &'static str) {
    rift_tracing::info!(
        component = "mcp",
        pid,
        stdin = "null",
        stdout = "null",
        stderr,
        "detached server spawned"
    );
}

/// Records that the detached server `pid` exited, with its exit code when this process
/// waited for it and the platform reported one.
fn record_exit(pid: Option<u32>, exit_code: Option<i32>) {
    rift_tracing::info!(component = "mcp", pid, exit_code, "spawned server exited");
}

/// The path of the detached server's standard error file below `root`.
#[must_use]
pub fn stderr_file_path(root: &Path) -> PathBuf {
    root.join(RIFT_STATE_DIRECTORY)
        .join(SERVER_STDERR_FILE_NAME)
}

/// Where a detached server's standard error goes: the workspace's stderr
/// file, truncated for this start, or nowhere when it cannot be created.
fn stderr_destination(root: &Path) -> Option<File> {
    match stderr_file(root) {
        Ok(file) => Some(file),
        Err(error) => {
            rift_tracing::warn!(
                component = "mcp",
                path = %stderr_file_path(root).display(),
                %error,
                "the server stderr file could not be created; the detached server's stderr is \
                 discarded"
            );
            None
        }
    }
}

/// Creates or truncates the stderr file, creating `.rift` when absent.
fn stderr_file(root: &Path) -> io::Result<File> {
    std::fs::create_dir_all(root.join(RIFT_STATE_DIRECTORY))?;
    File::create(stderr_file_path(root))
}

/// One detached server this process spawned.
#[derive(Debug)]
#[must_use = "a spawned server is watched through `is_running`"]
pub struct SpawnedServer {
    child: Child,
    /// The stderr file this start truncated for the child, when it could.
    stderr: Option<PathBuf>,
}

impl SpawnedServer {
    /// The child's process id, as the operator sees it in `ps`.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether the child has not exited yet.
    ///
    /// Never blocks: an exited child is reaped and answers `false`. A child
    /// whose state cannot be observed answers `false` too, so a caller
    /// never reports a server it cannot see as still starting; a running
    /// child it missed is found by the next probe through the election it
    /// holds.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

/// Spawns `rift server start --foreground` for `root`, fully detached,
/// with its startup stderr captured on a background thread.
///
/// `rift mcp` has no terminal of its own to show a failing spawn's
/// diagnostics: the caller polls the published lock document exactly as
/// [`spawn_detached_server`]'s callers do, and inspects the returned
/// [`StartupCapture`] when the child exits before that document appears.
///
/// # Errors
///
/// Returns the underlying failure when this binary's path cannot be read,
/// the process cannot be spawned, or its stderr pipe was not handed over.
pub(crate) fn spawn_detached_server_with_captured_stderr(
    root: &Path,
) -> Result<StartupCapture, io::Error> {
    let mut command = detached_command(root)?;
    command.stderr(Stdio::piped());
    let mut child = spawn_detached(&mut command)?;
    record_spawn(child.id(), "piped");
    let Some(stderr) = child.stderr.take() else {
        return Err(io::Error::other(
            "the spawned server's stderr pipe was not handed over",
        ));
    };
    let mut capture = StartupCapture::spawn(stderr);
    capture.pid = Some(child.id());
    Ok(capture)
}

/// A spawned server's captured standard error, read on a background
/// thread for as long as the pipe stays open.
///
/// The read loop never stops at the drain ceiling:
/// this process holds the pipe's only reader, and a spawned server that
/// starts successfully keeps running for the rest of the workspace's
/// life, writing to this same pipe. Stopping the read would eventually
/// fill the pipe and block the server's own writes; instead, bytes past
/// [`STARTUP_STDERR_CAPTURE_BYTES`] are read and discarded, and the
/// reported total caps at [`STREAM_TOTAL_BYTES_MAX`] as other captured streams do. The loop ends only at end-of-file, which in practice
/// means the child closed stderr because it exited.
///
/// A caller whose poll finds the server serving drops this value without
/// calling [`exited`](Self::exited) again: the background thread keeps
/// running and keeps discarding on its own, detached from anything this
/// process still holds. Once `rift mcp` itself exits, the pipe's read end
/// closes with it, and the daemon's later stderr writes fail with a
/// broken pipe rather than blocking - the daemon outlives the proxy, and
/// nothing reads its stderr again after that point.
#[derive(Debug)]
pub(crate) struct StartupCapture {
    drain: Option<std::thread::JoinHandle<CapturedStream>>,
    /// The spawned server's process identifier, when a spawn started the capture.
    pid: Option<u32>,
}

impl StartupCapture {
    /// Starts draining `stream` in the background.
    pub(crate) fn spawn(stream: impl Read + Send + 'static) -> Self {
        Self {
            drain: Some(std::thread::spawn(move || {
                drain_until_closed(stream, STARTUP_STDERR_CAPTURE_BYTES)
            })),
            pid: None,
        }
    }

    /// The captured stream once the child closed its end of the pipe - in
    /// practice, once it exited before publishing its lock document.
    /// `None`, and still draining in the background, while the pipe stays
    /// open.
    ///
    /// Never blocks: the background thread is joined only once it has
    /// already finished, so a caller may poll this from an async loop.
    pub(crate) fn exited(&mut self) -> Option<CapturedStream> {
        let finished = self
            .drain
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished);
        if !finished {
            return None;
        }
        self.drain.take().and_then(|handle| handle.join().ok())
    }
}

/// Reads `stream` to end-of-file, keeping the first `capture_bytes` and
/// counting the rest up to [`STREAM_TOTAL_BYTES_MAX`]. The loop never
/// stops early at that ceiling: see
/// [`StartupCapture`]'s doc comment for why it must keep reading.
fn drain_until_closed(mut stream: impl Read, capture_bytes: usize) -> CapturedStream {
    let mut kept: Vec<u8> = Vec::with_capacity(capture_bytes.min(STREAM_READ_BYTES));
    let mut total_bytes: u64 = 0;
    let mut buffer = [0_u8; STREAM_READ_BYTES];
    loop {
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

/// How one spawned server's start ended, once it exited.
#[derive(Debug)]
pub enum StartExit<Failure> {
    /// Its claim met a lock on the election file and it exited on its own,
    /// printing the `server_already_serving` refusal; carries what it printed.
    LostElection {
        /// The server's stderr, as far as the caller kept it.
        stderr: String,
    },
    /// It exited for any other reason; carries what the caller reports.
    Failed(Failure),
}

impl<Failure> StartExit<Failure> {
    /// The exit `stderr` names: a lost start election when it carries the
    /// `server_already_serving` refusal, and `failure` otherwise.
    fn classified(stderr: String, failure: Failure) -> Self {
        if lost_start_election(&stderr) {
            Self::LostElection { stderr }
        } else {
            Self::Failed(failure)
        }
    }
}

/// Whether a spawned server's stderr names a lost start election: its claim
/// met a lock on the election file - a concurrent starter's, or a probe's the
/// operating system has not released yet - and it exited on its own, printing
/// the same `server_already_serving` refusal an operator sees from `rift
/// server start --foreground`. The marker matches `server_already_serving`,
/// the CLI code for `rift.cli.server_already_serving`.
fn lost_start_election(stderr: &str) -> bool {
    stderr.contains("error[server_already_serving]")
}

/// A server one start spawned, watched until it exits.
pub trait StartedServer {
    /// What a failed start carries for the caller's report.
    type Failure: Debug;

    /// How the start ended once the server exited, and `None` while it runs.
    ///
    /// Never blocks, and answers an exit once: the caller keeps what it
    /// returned.
    fn observed_exit(&mut self) -> Option<StartExit<Self::Failure>>;
}

impl StartedServer for StartupCapture {
    type Failure = CapturedStream;

    /// The capture's end-of-file is the exit: the server closed its stderr.
    fn observed_exit(&mut self) -> Option<StartExit<CapturedStream>> {
        let capture = self.exited()?;
        record_exit(self.pid, None);
        Some(StartExit::classified(capture.text.clone(), capture))
    }
}

impl StartedServer for SpawnedServer {
    /// The exited child's pid.
    type Failure = u32;

    /// The child's exit status is the exit, and the tail of the stderr file
    /// this start truncated for it names the cause.
    fn observed_exit(&mut self) -> Option<StartExit<u32>> {
        let exit_code = match self.child.try_wait() {
            Ok(None) => return None,
            Ok(Some(status)) => status.code(),
            Err(_) => None,
        };
        record_exit(Some(self.pid()), exit_code);
        let stderr = self.stderr.as_deref().map(stderr_tail).unwrap_or_default();
        Some(StartExit::classified(stderr, self.pid()))
    }
}

/// The last [`EXIT_STDERR_TAIL_BYTES`] of the stderr file at `path`, or
/// nothing when the file cannot be read.
fn stderr_tail(path: &Path) -> String {
    let read = |path: &Path| -> io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let start = file
            .metadata()?
            .len()
            .saturating_sub(EXIT_STDERR_TAIL_BYTES);
        file.seek(SeekFrom::Start(start))?;
        let mut tail = Vec::new();
        file.take(EXIT_STDERR_TAIL_BYTES).read_to_end(&mut tail)?;
        Ok(tail)
    };
    read(path)
        .map(|tail| String::from_utf8_lossy(&tail).into_owned())
        .unwrap_or_default()
}

/// The detached servers one start spawned, and the latest one as the poll
/// last saw it.
///
/// `rift server start` and the stdio proxy start a workspace's server
/// through this one state: each spawns, then polls, and each poll weighs the
/// latest spawn's exit against the election a probe read after that exit.
#[derive(Debug)]
pub struct StartSpawns<Spawned: StartedServer> {
    latest: SpawnWatch<Spawned>,
    spawn_count: u32,
}

impl<Spawned: StartedServer> Default for StartSpawns<Spawned> {
    fn default() -> Self {
        Self {
            latest: SpawnWatch::Idle,
            spawn_count: 0,
        }
    }
}

/// The latest server one start spawned, as its poll last saw it.
#[derive(Debug)]
enum SpawnWatch<Spawned: StartedServer> {
    /// No spawned server is outstanding: none was spawned, the spawn could
    /// not launch, or the spawn count is spent.
    Idle,
    /// The spawned server has not exited yet, as far as the poll saw.
    Running(Spawned),
    /// The spawned server exited; a later poll weighs how.
    Exited(StartExit<Spawned::Failure>),
}

/// One poll iteration's outcome against a possibly still-spawning server.
#[derive(Debug)]
pub enum SpawnPollOutcome<Adopted, Failure> {
    /// The workspace's server answered.
    Ready(Adopted),
    /// The spawned server exited before it started serving, for a reason
    /// other than a lost election, and no process holds the election.
    Failed(Failure),
    /// The spawned server lost the start election, no process holds it, and
    /// the spawn count is not spent: nothing is left to publish, and the
    /// caller spawns again.
    ElectionUnheld,
    /// Nothing decided yet: the spawned server is still starting, or it
    /// exited while another process holds the election and may still
    /// publish.
    Waiting,
}

impl<Spawned: StartedServer> StartSpawns<Spawned> {
    /// Spawns one more server through `launch`, unless
    /// [`START_SPAWN_COUNT_MAX`] spawns already ran; a spent count spawns
    /// nothing, and the caller's poll waits out its window.
    ///
    /// # Errors
    ///
    /// Returns `launch`'s failure; the attempt still counts.
    pub fn spawn(&mut self, launch: impl FnOnce() -> io::Result<Spawned>) -> io::Result<()> {
        if self.is_spent() {
            debug_start_spawn("spawn skipped: count spent", self.spawn_count);
            self.latest = SpawnWatch::Idle;
            return Ok(());
        }
        self.spawn_count += 1;
        debug_start_spawn("spawn launch requested", self.spawn_count);
        match launch() {
            Ok(spawned) => {
                debug_start_spawn("spawn launch succeeded", self.spawn_count);
                self.latest = SpawnWatch::Running(spawned);
                Ok(())
            }
            Err(error) => {
                debug_start_spawn("spawn launch failed", self.spawn_count);
                self.latest = SpawnWatch::Idle;
                Err(error)
            }
        }
    }

    /// Whether [`START_SPAWN_COUNT_MAX`] spawns already ran.
    fn is_spent(&self) -> bool {
        self.spawn_count >= START_SPAWN_COUNT_MAX
    }

    /// The latest spawn, while the poll has not seen it exit.
    pub fn running(&mut self) -> Option<&mut Spawned> {
        match &mut self.latest {
            SpawnWatch::Running(spawned) => Some(spawned),
            SpawnWatch::Idle | SpawnWatch::Exited(_) => None,
        }
    }

    /// Classifies one poll iteration: an adopted server wins outright;
    /// otherwise the latest spawn's exit, weighed against the election,
    /// decides.
    ///
    /// `election_held` is what this iteration's probe found. It decides only
    /// for an exit an earlier iteration observed, so the probe that ends the
    /// wait or sends the caller to spawn again was read after the server
    /// exited.
    pub fn poll<Adopted>(
        &mut self,
        adopted: Option<Adopted>,
        election_held: bool,
    ) -> SpawnPollOutcome<Adopted, Spawned::Failure> {
        if let Some(adopted) = adopted {
            debug_start_spawn("poll ready", self.spawn_count);
            return SpawnPollOutcome::Ready(adopted);
        }
        match std::mem::replace(&mut self.latest, SpawnWatch::Idle) {
            SpawnWatch::Running(mut spawned) => {
                self.latest = match spawned.observed_exit() {
                    Some(exit) => SpawnWatch::observed(exit),
                    None => SpawnWatch::Running(spawned),
                };
                debug_start_spawn("poll waiting on running child", self.spawn_count);
                SpawnPollOutcome::Waiting
            }
            SpawnWatch::Exited(exit) if election_held => {
                self.latest = SpawnWatch::Exited(exit);
                debug_start_spawn("poll waiting on held election", self.spawn_count);
                SpawnPollOutcome::Waiting
            }
            SpawnWatch::Exited(StartExit::LostElection { .. }) if self.is_spent() => {
                let spawn_count = self.spawn_count;
                rift_tracing::warn!(
                    component = "mcp",
                    spawn_count,
                    "the spawn count is spent; the start window passes as a wait"
                );
                debug_start_spawn("poll waiting: spawn count spent", self.spawn_count);
                SpawnPollOutcome::Waiting
            }
            SpawnWatch::Exited(StartExit::LostElection { .. }) => {
                rift_tracing::info!(
                    component = "mcp",
                    "no process holds the election the spawned server lost; spawning again"
                );
                debug_start_spawn("poll election unheld", self.spawn_count);
                SpawnPollOutcome::ElectionUnheld
            }
            SpawnWatch::Exited(StartExit::Failed(failure)) => {
                debug_start_spawn("poll failed", self.spawn_count);
                SpawnPollOutcome::Failed(failure)
            }
            SpawnWatch::Idle => {
                debug_start_spawn("poll idle", self.spawn_count);
                SpawnPollOutcome::Waiting
            }
        }
    }
}

impl<Spawned: StartedServer> SpawnWatch<Spawned> {
    /// Records an observed exit, naming a lost election with what the server
    /// printed.
    fn observed(exit: StartExit<Spawned::Failure>) -> Self {
        if let StartExit::LostElection { stderr } = &exit {
            debug_start_child_stderr(stderr);
            rift_tracing::info!(
                component = "mcp",
                stderr = %stderr,
                "the spawned server lost the start election"
            );
        }
        Self::Exited(exit)
    }
}

impl StartSpawns<StartupCapture> {
    /// Spawns one more detached server with its stderr captured, reporting a
    /// spawn that cannot launch: the poll still gives a concurrently started
    /// server its chance.
    pub(crate) fn spawn_captured(&mut self, root: &Path) {
        if let Err(error) = self.spawn(|| spawn_detached_server_with_captured_stderr(root)) {
            rift_tracing::warn!(component = "mcp", %error, "detached server spawn failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{
        CapturedStream, EXIT_STDERR_TAIL_BYTES, PRESENCE_POLL_INTERVAL, START_POLL_ATTEMPT_COUNT,
        START_SPAWN_COUNT_MAX, START_WAIT_MAX, STARTUP_STDERR_CAPTURE_BYTES, SpawnPollOutcome,
        SpawnWatch, StartExit, StartSpawns, StartupCapture, lost_start_election, stderr_file_path,
        stderr_tail,
    };
    #[cfg(unix)]
    use super::{SpawnedServer, StartedServer};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    /// Poll attempts while waiting on a spawned child to exit: 10 seconds.
    const EXIT_POLL_ATTEMPT_COUNT: u32 = 100;

    /// Spawns `sh -c script` in the detached shape, the way
    /// `spawn_detached_server` spawns a server, with its stderr on the
    /// workspace's stderr file.
    #[cfg(unix)]
    fn spawn_detached_script(root: &std::path::Path, script: &str) -> TestResult<SpawnedServer> {
        let command = super::detached_command_for("sh", ["-c", script], root);
        Ok(super::spawn_with_stderr_file(command, root)?)
    }

    /// Runs `script` as [`spawn_detached_script`] does and waits for it to exit.
    #[cfg(unix)]
    fn run_detached_script(root: &std::path::Path, script: &str) -> TestResult<SpawnedServer> {
        let mut spawned = spawn_detached_script(root, script)?;
        assert!(spawned.pid() > 0);
        for _ in 0..EXIT_POLL_ATTEMPT_COUNT {
            if !spawned.is_running() {
                return Ok(spawned);
            }
            std::thread::sleep(PRESENCE_POLL_INTERVAL);
        }
        Err("the detached script must exit".into())
    }

    #[cfg(unix)]
    #[test]
    fn a_detached_child_writes_its_stderr_to_the_workspace_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _first = run_detached_script(directory.path(), "echo first start >&2")?;
        assert_eq!(
            std::fs::read_to_string(stderr_file_path(directory.path()))?,
            "first start\n"
        );

        let _second = run_detached_script(directory.path(), "echo second start >&2")?;
        assert_eq!(
            std::fs::read_to_string(stderr_file_path(directory.path()))?,
            "second start\n",
            "each start truncates the file"
        );
        Ok(())
    }

    /// A spawn records the child's process identifier and where each of its standard
    /// streams goes, and its observed exit records the same identifier and its exit code.
    #[cfg(unix)]
    #[test]
    fn a_detached_child_records_its_spawn_and_its_exit() -> TestResult {
        let directory = tempfile::tempdir()?;
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
        let mut spawned = run_detached_script(directory.path(), "exit 3")?;
        assert!(spawned.observed_exit().is_some(), "the script exited");
        drop(recorder);

        let records = drain.queued_records();
        let fields_of = |message: &str| -> TestResult<serde_json::Value> {
            let mut found = records.iter().filter(|record| record.message() == message);
            let record = found.next().ok_or(format!("{message}: {records:?}"))?;
            assert!(found.next().is_none(), "one {message} record");
            assert_eq!(record.level(), "info");
            Ok(serde_json::from_str(record.fields())?)
        };
        let pid = spawned.pid().to_string();
        let spawn = fields_of("detached server spawned")?;
        assert_eq!(spawn["pid"], pid, "{spawn}");
        assert_eq!(spawn["stdin"], "null", "{spawn}");
        assert_eq!(spawn["stdout"], "null", "{spawn}");
        assert_eq!(spawn["stderr"], "file", "{spawn}");
        let exit = fields_of("spawned server exited")?;
        assert_eq!(exit["pid"], pid, "{exit}");
        assert_eq!(exit["exit_code"], "3", "{exit}");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_state_directory_discards_the_child_stderr_and_still_spawns() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join(".rift"), b"a file in the way")?;
        let mut discarded = run_detached_script(directory.path(), "echo lost >&2")?;
        assert!(!stderr_file_path(directory.path()).exists());
        assert!(
            matches!(discarded.observed_exit(), Some(StartExit::Failed(_))),
            "an exit with no stderr file to read is a failure, never a lost election"
        );
        Ok(())
    }

    #[test]
    fn start_poll_attempt_count_derives_from_its_window() {
        assert!(
            START_WAIT_MAX >= Duration::from_secs(30),
            "startup includes identity, watch setup, initial catalog, and lexical publication"
        );
        assert_eq!(
            PRESENCE_POLL_INTERVAL * START_POLL_ATTEMPT_COUNT,
            START_WAIT_MAX
        );
    }

    /// A test double whose reads block on a channel, so a test controls
    /// exactly when the simulated pipe closes: sending bytes makes them
    /// available to read, and dropping the sender yields end-of-file. A
    /// sent message larger than one read buffer is retained across calls,
    /// the same way a real pipe's bytes are - a caller that copied only
    /// the first read's worth into `Ok(taken)` and dropped the rest would
    /// silently shrink every oversized message.
    struct BlockingChannelStream {
        receiver: mpsc::Receiver<Vec<u8>>,
        pending: Vec<u8>,
    }

    impl BlockingChannelStream {
        fn new(receiver: mpsc::Receiver<Vec<u8>>) -> Self {
            Self {
                receiver,
                pending: Vec::new(),
            }
        }
    }

    impl std::io::Read for BlockingChannelStream {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.pending.is_empty() {
                match self.receiver.recv() {
                    Ok(bytes) => self.pending = bytes,
                    Err(_closed) => return Ok(0),
                }
            }
            let taken = self.pending.len().min(buffer.len());
            buffer[..taken].copy_from_slice(&self.pending[..taken]);
            self.pending.drain(..taken);
            Ok(taken)
        }
    }

    /// Polls `capture` until it reports the child exited, bounded so a
    /// defect in the drain thread fails the test instead of hanging it.
    fn wait_for_exit(capture: &mut StartupCapture) -> CapturedStream {
        for _ in 0..1_000 {
            if let Some(captured) = capture.exited() {
                return captured;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("the background drain must finish once the stream closes");
    }

    #[test]
    fn test_exited_is_none_while_the_pipe_stays_open() {
        let (_sender, receiver) = mpsc::channel();
        let mut capture = StartupCapture::spawn(BlockingChannelStream::new(receiver));
        assert!(
            capture.exited().is_none(),
            "an open pipe must report no capture yet"
        );
    }

    #[test]
    fn test_exited_reports_the_captured_stream_once_the_pipe_closes() {
        let (sender, receiver) = mpsc::channel();
        let mut capture = StartupCapture::spawn(BlockingChannelStream::new(receiver));
        sender
            .send(b"server failed to bind its port".to_vec())
            .expect("receiver still open");
        drop(sender);
        let captured = wait_for_exit(&mut capture);
        assert_eq!(captured.text, "server failed to bind its port");
        assert!(!captured.truncated);
    }

    #[test]
    fn test_captured_stream_keeps_only_the_configured_prefix() {
        let (sender, receiver) = mpsc::channel();
        let mut capture = StartupCapture::spawn(BlockingChannelStream::new(receiver));
        let overrun = vec![b'x'; STARTUP_STDERR_CAPTURE_BYTES + 64];
        sender.send(overrun.clone()).expect("receiver still open");
        drop(sender);
        let captured = wait_for_exit(&mut capture);
        assert_eq!(captured.captured_bytes, STARTUP_STDERR_CAPTURE_BYTES as u64);
        assert_eq!(captured.total_bytes, overrun.len() as u64);
        assert!(captured.truncated);
    }

    #[test]
    fn test_exited_is_idempotently_none_after_being_taken() {
        let (sender, receiver) = mpsc::channel();
        let mut capture = StartupCapture::spawn(BlockingChannelStream::new(receiver));
        drop(sender);
        let _first = wait_for_exit(&mut capture);
        assert_eq!(
            capture.exited(),
            None,
            "a capture already taken must not be reported twice"
        );
    }

    /// The refusal a spawned server prints when it loses the start election.
    const LOST_ELECTION_STDERR: &[u8] = b"rift: error[server_already_serving]: another rift \
        server already serves this workspace; connect to the listed server, or run `rift server \
        stop` before serving again";

    /// Spawns watched through `stream` alone: one spawn ran, and its stderr is still draining.
    fn watching(stream: BlockingChannelStream) -> StartSpawns<StartupCapture> {
        StartSpawns {
            latest: SpawnWatch::Running(StartupCapture::spawn(stream)),
            spawn_count: 1,
        }
    }

    /// Spawns watched through a stream that already carried `stderr` and closed.
    fn exited_with(stderr: &[u8]) -> StartSpawns<StartupCapture> {
        let (sender, receiver) = mpsc::channel::<Vec<u8>>();
        let spawns = watching(BlockingChannelStream::new(receiver));
        sender.send(stderr.to_vec()).expect("receiver still open");
        drop(sender);
        spawns
    }

    /// Polls `spawns` with no adoption until the latest spawn's exit is observed, asserting
    /// every poll before it waited; bounded so a defect in the drain fails instead of hanging.
    fn poll_until_exit_observed(spawns: &mut StartSpawns<StartupCapture>, election_held: bool) {
        for _ in 0..1_000 {
            if spawns.running().is_none() {
                return;
            }
            let outcome = spawns.poll::<u32>(None, election_held);
            assert!(
                matches!(outcome, SpawnPollOutcome::Waiting),
                "a poll that observes the exit waits"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the background drain must finish once the stream closes");
    }

    #[test]
    fn spawn_poll_prefers_adoption_over_a_finished_capture() {
        let mut spawns = exited_with(b"");
        // The capture has finished (the stream closed), and adoption also
        // succeeded on this same iteration: adoption must win, and the
        // capture must never be consulted.
        let startup = spawns.running().expect("the capture must still be present");
        let _ = wait_for_exit(startup);
        let outcome = spawns.poll(Some(7_u32), false);
        assert!(
            matches!(outcome, SpawnPollOutcome::Ready(7)),
            "adoption must win over a finished capture"
        );
    }

    #[test]
    fn spawn_poll_waits_while_the_capture_is_still_open() {
        let (_sender, receiver) = mpsc::channel::<Vec<u8>>();
        let mut spawns = watching(BlockingChannelStream::new(receiver));
        let outcome = spawns.poll::<u32>(None, false);
        assert!(matches!(outcome, SpawnPollOutcome::Waiting));
    }

    /// A spawned server that exited for another reason fails the poll once a probe read
    /// after the exit finds the election unheld, with what the server printed.
    #[test]
    fn spawn_poll_fails_once_the_spawned_server_exited_and_nobody_holds() {
        let mut spawns = exited_with(b"listener bind failed: address in use");
        poll_until_exit_observed(&mut spawns, false);
        let SpawnPollOutcome::Failed(capture) = spawns.poll::<u32>(None, false) else {
            panic!("an exited spawn under a free election must fail the poll");
        };
        assert_eq!(capture.text, "listener bind failed: address in use");
    }

    /// A failed spawn under another holder's election waits for that holder, which may
    /// still publish; the failure stands once the election is free.
    #[test]
    fn spawn_poll_waits_for_a_holder_after_a_failed_exit() {
        let mut spawns = exited_with(b"listener bind failed: address in use");
        poll_until_exit_observed(&mut spawns, true);
        assert!(
            matches!(spawns.poll::<u32>(None, true), SpawnPollOutcome::Waiting),
            "a held election waits for its holder"
        );
        assert!(
            matches!(spawns.poll::<u32>(None, false), SpawnPollOutcome::Failed(_)),
            "the failure stands once the election is free"
        );
    }

    /// A lost election never fails the poll: while another process holds the
    /// election, the winner may still be binding, and the poll keeps waiting
    /// for it.
    #[test]
    fn spawn_poll_keeps_waiting_while_the_election_the_spawned_server_lost_is_held() {
        let mut spawns = exited_with(LOST_ELECTION_STDERR);
        poll_until_exit_observed(&mut spawns, true);
        for _ in 0..200 {
            let outcome = spawns.poll::<u32>(None, true);
            assert!(
                matches!(outcome, SpawnPollOutcome::Waiting),
                "a spawned server that lost the election to a live holder must keep the poll \
                 waiting for the winner, not fail it"
            );
        }
    }

    /// A lost election that leaves the election unheld has no winner to wait
    /// for, and the poll says so - but only on a probe read after the loss
    /// was observed, since the probe of the observing iteration may predate
    /// the losing server's exit.
    #[test]
    fn spawn_poll_names_an_unheld_election_only_after_the_loss_was_observed() {
        let mut spawns = exited_with(LOST_ELECTION_STDERR);
        poll_until_exit_observed(&mut spawns, false);
        assert!(
            matches!(
                spawns.latest,
                SpawnWatch::Exited(StartExit::LostElection { .. })
            ),
            "the observed loss is recorded"
        );
        assert!(
            matches!(
                spawns.poll::<u32>(None, false),
                SpawnPollOutcome::ElectionUnheld
            ),
            "a later probe that finds the election unheld must send the start to spawn again"
        );
    }

    /// Below the bound a spawn counts even when it cannot launch; at the bound
    /// nothing launches, and the poll waits out the window.
    #[test]
    fn the_spawn_count_bounds_the_spawns_one_start_makes() {
        let mut spawns = StartSpawns::<StartupCapture> {
            latest: SpawnWatch::Exited(StartExit::LostElection {
                stderr: String::new(),
            }),
            spawn_count: START_SPAWN_COUNT_MAX - 1,
        };
        let refused = spawns.spawn(|| Err(std::io::Error::other("no such program")));
        assert!(refused.is_err(), "a launch failure reaches the caller");
        assert_eq!(spawns.spawn_count, START_SPAWN_COUNT_MAX);
        assert!(
            spawns.running().is_none(),
            "a spawn that cannot launch leaves nothing to watch"
        );

        let spent = spawns.spawn(|| panic!("a spent count must not launch"));
        assert!(spent.is_ok(), "a spent count is no failure of its own");
        assert_eq!(spawns.spawn_count, START_SPAWN_COUNT_MAX);
        assert!(
            matches!(spawns.poll::<u32>(None, false), SpawnPollOutcome::Waiting),
            "with the count spent the poll only waits"
        );
    }

    /// The poll that finds an unheld election with the spawn count spent says the count is
    /// spent and waits; it neither asks for another spawn nor announces one.
    #[test]
    fn a_spent_spawn_count_waits_without_announcing_another_spawn() {
        let mut spawns = StartSpawns::<StartupCapture> {
            latest: SpawnWatch::Exited(StartExit::LostElection {
                stderr: String::new(),
            }),
            spawn_count: START_SPAWN_COUNT_MAX,
        };
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("the default filter parses");
        let outcome = spawns.poll::<u32>(None, false);
        drop(recorder);

        assert!(
            matches!(outcome, SpawnPollOutcome::Waiting),
            "a spent count asks for no spawn: {outcome:?}"
        );
        let messages: Vec<String> = drain
            .queued_records()
            .iter()
            .map(|record| record.message().to_owned())
            .collect();
        assert_eq!(
            messages,
            ["the spawn count is spent; the start window passes as a wait"],
            "the spent poll names the count once and announces no spawn"
        );
    }

    #[test]
    fn lost_start_election_recognizes_the_server_already_serving_marker() {
        assert!(lost_start_election(&String::from_utf8_lossy(
            LOST_ELECTION_STDERR
        )));
    }

    #[test]
    fn lost_start_election_rejects_an_unrelated_startup_failure() {
        assert!(!lost_start_election("listener bind failed: address in use"));
    }

    /// A detached server's exit is read from its status, and the stderr file this start
    /// truncated for it names why: the refusal it exits on is the last thing it writes.
    #[cfg(unix)]
    #[test]
    fn a_detached_exit_is_classified_from_the_tail_of_its_stderr_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let lost = format!(
            "printf 'starting\\n{}\\n' >&2",
            String::from_utf8_lossy(LOST_ELECTION_STDERR)
        );
        let mut loser = run_detached_script(directory.path(), &lost)?;
        let Some(StartExit::LostElection { stderr }) = loser.observed_exit() else {
            panic!("the refusal in the stderr file names a lost election");
        };
        assert!(stderr.contains("server_already_serving"), "{stderr}");

        let mut failed = run_detached_script(directory.path(), "echo bind failed >&2; exit 1")?;
        let pid = failed.pid();
        assert!(
            matches!(failed.observed_exit(), Some(StartExit::Failed(exited)) if exited == pid),
            "any other exit fails with the child's pid"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_running_detached_server_has_no_exit_yet() -> TestResult {
        let directory = tempfile::tempdir()?;
        let mut running = spawn_detached_script(directory.path(), "sleep 30")?;
        let observed = running.observed_exit();
        running.child.kill()?;
        running.child.wait()?;
        assert!(
            observed.is_none(),
            "a running child has no exit to classify"
        );
        Ok(())
    }

    #[test]
    fn the_stderr_tail_keeps_the_last_bytes_and_reads_nothing_from_a_missing_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("server.stderr");
        assert_eq!(stderr_tail(&path), "", "a missing file reads as nothing");

        let head = "a".repeat(usize::try_from(EXIT_STDERR_TAIL_BYTES)?);
        std::fs::write(&path, format!("{head}the last line\n"))?;
        let tail = stderr_tail(&path);
        assert_eq!(tail.len(), usize::try_from(EXIT_STDERR_TAIL_BYTES)?);
        assert!(tail.ends_with("the last line\n"), "{tail:?}");
        Ok(())
    }
}
