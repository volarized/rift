//! Detached spawning of one workspace's `rift server` process.
//!
//! The CLI's `rift server start` and the stdio proxy start the workspace's
//! server the same way: this binary again, as `rift server start
//! --foreground`, fully detached from the caller's terminal and process
//! group. The poll constants for waiting on the spawned server's published
//! lock document live beside the spawn, so every caller shares one meaning
//! of "the server came up in time".

use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use rift_core::{CapturedStream, STREAM_READ_BYTES, STREAM_TOTAL_BYTES_MAX};

/// Bytes of a detached server's startup stderr kept verbatim; the rest is
/// only counted, the same split [`CapturedStream`] reports for a hook's
/// captured streams.
const STARTUP_STDERR_CAPTURE_BYTES: usize = 8 << 10;

/// Pause between presence probes while waiting on a workspace's server.
pub const PRESENCE_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Longest wait for a spawned server to publish its lock document.
///
/// A start poll runs `START_WAIT_MAX / PRESENCE_POLL_INTERVAL` = 150
/// bounded iterations.
pub const START_WAIT_MAX: Duration = Duration::from_secs(15);
/// Probe attempts one start waits: [`START_WAIT_MAX`] over the interval.
pub const START_POLL_ATTEMPT_COUNT: u32 = 150;

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
    let mut command = Command::new(program);
    command
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    detach(&mut command);
    Ok(command)
}

/// Spawns `rift server start --foreground` for `root`, fully detached,
/// discarding its stderr entirely.
///
/// The child runs this same binary with `root` as its working directory and
/// inherits this process's environment - it serves the workspace the caller
/// addressed - with stdin, stdout, and stderr all null and its own process
/// group, so it survives the caller's exit and its terminal. The child
/// handle is dropped unawaited: callers poll the published lock document
/// instead, and an exited child is reaped by the init process.
///
/// Losing the election race is not a spawn failure: a child that finds the
/// workspace already served exits on its own, and the caller's poll adopts
/// whoever won.
///
/// Used by `rift server start`: the operator already sees the elected
/// server's own diagnostics through `--foreground`'s direct process, so
/// this detached spawn only needs to start the workspace's background
/// server. `rift mcp` has no such channel and uses
/// `spawn_detached_server_with_captured_stderr` instead.
///
/// # Errors
///
/// Returns the underlying failure when this binary's path cannot be read
/// or the process cannot be spawned; callers classify it for their own
/// surface.
pub fn spawn_detached_server(root: &Path) -> Result<(), io::Error> {
    let mut command = detached_command(root)?;
    command.stderr(Stdio::null());
    command.spawn().map(drop)
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
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{
        CapturedStream, PRESENCE_POLL_INTERVAL, START_POLL_ATTEMPT_COUNT, START_WAIT_MAX,
        STARTUP_STDERR_CAPTURE_BYTES, StartupCapture,
    };

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
