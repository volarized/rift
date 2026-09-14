//! Detached spawning of one workspace's `rift server` process.
//!
//! The CLI's `rift server start` and the stdio proxy start the workspace's
//! server the same way: this binary again, as `rift server start
//! --foreground`, fully detached from the caller's terminal and process
//! group. The poll constants for waiting on the spawned server's published
//! lock document live beside the spawn, so every caller shares one meaning
//! of "the server came up in time".

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rift_core::constants::RIFT_STATE_DIRECTORY;
use rift_core::{CapturedStream, STREAM_READ_BYTES, STREAM_TOTAL_BYTES_MAX};
use tracing_subscriber::fmt::MakeWriter;

/// Bytes of a detached server's startup stderr kept verbatim; the rest is
/// only counted, the same split [`CapturedStream`] reports for a hook's
/// captured streams.
const STARTUP_STDERR_CAPTURE_BYTES: usize = 8 << 10;

/// The file under `.rift`, beside `server.json`, that holds the standard
/// error of the server `rift server start` spawns. Each start truncates it.
pub const SERVER_STDERR_FILE_NAME: &str = "server.stderr";
/// Bytes of traced diagnostics a server writes to its standard error before
/// it stops writing there, when that stream is not a terminal.
///
/// The file is what a crashed server leaves behind: it holds the start, and
/// a panic's own report reaches it through the default panic hook past this
/// bound. The diagnostics of a long life go to `rift server logs`.
pub const SERVER_STDERR_BYTES_MAX: u64 = 1 << 20;
/// The line the writer prints once, as the last thing, when the bound is reached.
const SERVER_STDERR_BOUND_NOTICE: &str =
    "rift: standard error reached its byte bound; later diagnostics are under `rift server logs`\n";

/// Pause between presence probes while waiting on a workspace's server.
pub const PRESENCE_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Longest wait for a spawned server to publish its lock document.
///
/// A start poll runs `START_WAIT_MAX / PRESENCE_POLL_INTERVAL` = 300
/// bounded iterations.
pub const START_WAIT_MAX: Duration = Duration::from_secs(30);
/// Probe attempts one start waits: [`START_WAIT_MAX`] over the interval.
pub const START_POLL_ATTEMPT_COUNT: u32 = 300;

/// Keeps a detached child completely off this process's terminal and
/// process group (unix half).
#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

/// `CreateProcess` flag detaching the child from the parent console.
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x8;
/// `CreateProcess` flag giving the child its own signal group.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;

/// Keeps a detached child completely off this process's console and
/// process group (windows half).
#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

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

/// The detached shape over any program: run inside `root`, off this
/// process's terminal and process group, stdin and stdout null.
fn detached_command_for(
    program: impl AsRef<OsStr>,
    arguments: impl IntoIterator<Item = impl AsRef<OsStr>>,
    root: &Path,
) -> Command {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    detach(&mut command);
    command
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
    let mut command = detached_command(root)?;
    command.stderr(stderr_destination(root));
    let child = command.spawn()?;
    Ok(SpawnedServer { child })
}

/// The path of the detached server's standard error file below `root`.
#[must_use]
pub fn stderr_file_path(root: &Path) -> PathBuf {
    root.join(RIFT_STATE_DIRECTORY)
        .join(SERVER_STDERR_FILE_NAME)
}

/// Where a detached server's standard error goes: the workspace's stderr
/// file, truncated for this start, or nowhere when it cannot be created.
fn stderr_destination(root: &Path) -> Stdio {
    match stderr_file(root) {
        Ok(file) => Stdio::from(file),
        Err(error) => {
            tracing::warn!(
                component = "mcp",
                path = %stderr_file_path(root).display(),
                %error,
                "the server stderr file could not be created; the detached server's stderr is \
                 discarded"
            );
            Stdio::null()
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

/// Standard error of a server whose stream is a file, cut at
/// [`SERVER_STDERR_BYTES_MAX`].
///
/// The file `rift server start` hands its server would otherwise grow for
/// the server's whole life. Past the bound the writer prints one notice and
/// drops what it is handed afterwards; the diagnostics recorded under
/// `rift server logs` are unaffected.
#[derive(Debug, Default)]
pub struct BoundedStderr {
    written: AtomicU64,
}

impl<'a> MakeWriter<'a> for BoundedStderr {
    type Writer = BoundedWriter<'a, io::Stderr>;

    fn make_writer(&'a self) -> Self::Writer {
        BoundedWriter::new(&self.written, io::stderr())
    }
}

/// One writer over a shared byte count: writes pass through until the count
/// reaches [`SERVER_STDERR_BYTES_MAX`], the crossing write is followed by
/// the notice, and later writes are counted and dropped.
#[derive(Debug)]
pub struct BoundedWriter<'a, Sink: Write> {
    written: &'a AtomicU64,
    sink: Sink,
}

impl<'a, Sink: Write> BoundedWriter<'a, Sink> {
    /// A writer over `sink` sharing `written` with every sibling writer.
    pub fn new(written: &'a AtomicU64, sink: Sink) -> Self {
        Self { written, sink }
    }
}

impl<Sink: Write> Write for BoundedWriter<'_, Sink> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let before = self
            .written
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        if before >= SERVER_STDERR_BYTES_MAX {
            return Ok(bytes.len());
        }
        self.sink.write_all(bytes)?;
        if before + bytes.len() as u64 >= SERVER_STDERR_BYTES_MAX {
            self.sink.write_all(SERVER_STDERR_BOUND_NOTICE.as_bytes())?;
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sink.flush()
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
    let mut child = command.spawn()?;
    let Some(stderr) = child.stderr.take() else {
        return Err(io::Error::other(
            "the spawned server's stderr pipe was not handed over",
        ));
    };
    Ok(StartupCapture::spawn(stderr))
}

/// A spawned server's captured standard error, read on a background
/// thread for as long as the pipe stays open.
///
/// Unlike a hook capture, the read loop never stops at the drain ceiling:
/// this process holds the pipe's only reader, and a spawned server that
/// starts successfully keeps running for the rest of the workspace's
/// life, writing to this same pipe. Stopping the read would eventually
/// fill the pipe and block the server's own writes; instead, bytes past
/// [`STARTUP_STDERR_CAPTURE_BYTES`] are read and discarded, and the
/// reported total caps at [`STREAM_TOTAL_BYTES_MAX`] the same way a hook
/// capture's does. The loop ends only at end-of-file, which in practice
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
}

impl StartupCapture {
    /// Starts draining `stream` in the background.
    pub(crate) fn spawn(stream: impl Read + Send + 'static) -> Self {
        Self {
            drain: Some(std::thread::spawn(move || {
                drain_until_closed(stream, STARTUP_STDERR_CAPTURE_BYTES)
            })),
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
/// stops early at that ceiling the way a hook capture's does: see
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

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{
        BoundedWriter, CapturedStream, PRESENCE_POLL_INTERVAL, SERVER_STDERR_BOUND_NOTICE,
        SERVER_STDERR_BYTES_MAX, START_POLL_ATTEMPT_COUNT, START_WAIT_MAX,
        STARTUP_STDERR_CAPTURE_BYTES, StartupCapture, stderr_file_path,
    };

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    /// Poll attempts while waiting on a spawned child to exit: 10 seconds.
    const EXIT_POLL_ATTEMPT_COUNT: u32 = 100;

    /// Spawns `sh -c script` in the detached shape with its stderr on the
    /// workspace's stderr file, and waits for it to exit.
    #[cfg(unix)]
    fn run_detached_script(root: &std::path::Path, script: &str) -> TestResult {
        let mut command = super::detached_command_for("sh", ["-c", script], root);
        command.stderr(super::stderr_destination(root));
        let mut spawned = super::SpawnedServer {
            child: command.spawn()?,
        };
        assert!(spawned.pid() > 0);
        for _ in 0..EXIT_POLL_ATTEMPT_COUNT {
            if !spawned.is_running() {
                return Ok(());
            }
            std::thread::sleep(PRESENCE_POLL_INTERVAL);
        }
        Err("the detached script must exit".into())
    }

    #[cfg(unix)]
    #[test]
    fn a_detached_child_writes_its_stderr_to_the_workspace_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        run_detached_script(directory.path(), "echo first start >&2")?;
        assert_eq!(
            std::fs::read_to_string(stderr_file_path(directory.path()))?,
            "first start\n"
        );

        run_detached_script(directory.path(), "echo second start >&2")?;
        assert_eq!(
            std::fs::read_to_string(stderr_file_path(directory.path()))?,
            "second start\n",
            "each start truncates the file"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_state_directory_discards_the_child_stderr_and_still_spawns() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join(".rift"), b"a file in the way")?;
        run_detached_script(directory.path(), "echo lost >&2")?;
        assert!(!stderr_file_path(directory.path()).exists());
        Ok(())
    }

    #[test]
    fn a_bounded_writer_passes_the_crossing_write_then_drops() -> TestResult {
        let written = AtomicU64::new(0);
        let mut sink = Vec::new();
        let head = vec![b'a'; usize::try_from(SERVER_STDERR_BYTES_MAX)? - 4];
        {
            let mut writer = BoundedWriter::new(&written, &mut sink);
            writer.write_all(&head)?;
            writer.write_all(b"crossing")?;
            writer.write_all(b"dropped")?;
            writer.flush()?;
        }
        let expected_length = head.len() + "crossing".len() + SERVER_STDERR_BOUND_NOTICE.len();
        assert_eq!(sink.len(), expected_length);
        assert!(sink.ends_with(SERVER_STDERR_BOUND_NOTICE.as_bytes()));
        assert!(!sink.windows(7).any(|window| window == b"dropped"));
        assert_eq!(
            written.load(std::sync::atomic::Ordering::Relaxed),
            (head.len() + "crossing".len() + "dropped".len()) as u64,
            "dropped bytes are still counted"
        );
        Ok(())
    }

    #[test]
    fn a_bounded_writer_shares_its_count_between_writers() -> TestResult {
        let written = AtomicU64::new(SERVER_STDERR_BYTES_MAX);
        let mut sink = Vec::new();
        BoundedWriter::new(&written, &mut sink).write_all(b"late")?;
        assert!(sink.is_empty(), "a writer past the bound writes nothing");
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
}
