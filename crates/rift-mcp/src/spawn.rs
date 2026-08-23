//! Detached spawning of one workspace's `rift server` process.
//!
//! The CLI's `rift server start` and the stdio proxy start the workspace's
//! server the same way: this binary again, as `rift server start
//! --foreground`, fully detached from the caller's terminal and process
//! group. The poll constants for waiting on the spawned server's published
//! lock document live beside the spawn, so every caller shares one meaning
//! of "the server came up in time".

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

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

/// Spawns `rift server start --foreground` for `root`, fully detached.
///
/// The child runs this same binary with `root` as its working directory and
/// inherits this process's environment — it serves the workspace the caller
/// addressed — with stdin, stdout, and stderr all null and its own process
/// group, so it survives the caller's exit and its terminal. The child
/// handle is dropped unawaited: callers poll the published lock document
/// instead, and an exited child is reaped by the init process.
///
/// Losing the election race is not a spawn failure: a child that finds the
/// workspace already served exits on its own, and the caller's poll adopts
/// whoever won.
///
/// # Errors
///
/// Returns the underlying failure when this binary's path cannot be read
/// or the process cannot be spawned; callers classify it for their own
/// surface.
pub fn spawn_detached_server(root: &Path) -> Result<(), io::Error> {
    let program = std::env::current_exe()?;
    let mut command = Command::new(program);
    command
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    command.spawn().map(drop)
}

#[cfg(test)]
mod tests {
    use super::{PRESENCE_POLL_INTERVAL, START_POLL_ATTEMPT_COUNT, START_WAIT_MAX};

    #[test]
    fn start_poll_attempt_count_derives_from_its_window() {
        assert_eq!(
            PRESENCE_POLL_INTERVAL * START_POLL_ATTEMPT_COUNT,
            START_WAIT_MAX
        );
    }
}
