//! Real-binary contract of `rift server start|stop|restart|status`.
//!
//! Every test drives the compiled `rift` binary against a throwaway
//! workspace fixture. The tests serialize on one mutex: the servers share
//! the loopback election port range, and serial runs keep "the old port is
//! free again" assertions honest. Each fixture's `rift.toml` accepts a
//! 60-second idle timeout as an orphan-safety net, and a drop guard stops
//! any server a failed test leaves behind.

use std::error::Error;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use rift_mcp::{START_POLL_ATTEMPT_COUNT, ServerPresence, probe};
use rift_protocol::lock::{
    ProductIdentity, SERVER_LOCK_FILE_NAME, SERVER_TOKEN_LENGTH, ServerLock,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Pause between polls of any awaited condition.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Poll attempts while waiting on a server to disappear, a child to exit,
/// or a port to refuse again: 10 seconds.
const GONE_POLL_ATTEMPT_COUNT: u32 = 100;
/// Bound on one probing TCP connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// Concurrent `rift server start` invocations in the election race.
const CONCURRENT_START_COUNT: usize = 4;

/// Serializes the tests: the served port range is machine-global.
static SERIAL: Mutex<()> = Mutex::new(());

fn stale_identity() -> ProductIdentity {
    ProductIdentity {
        version: "0.0.1".to_owned(),
        executable_digest: "a".repeat(64),
        schema_digest: "b".repeat(64),
    }
}

/// The `[search.semantic]` table every fixture here declares.
///
/// Rift ships the semantic tier on, so a fixture carrying no such table would acquire
/// the default model from the hub. A hermetic suite must not write into the developer's
/// own Hugging Face cache, and on a runner with no network a default-on tier would spend
/// its whole retry budget inside a detached task nobody waits on. `rift-mcp`'s
/// `tests/hermetic_search.rs` states the same policy for that crate's suites, and its
/// `live_semantic_search` suite is the one place the shipped default is exercised.
const SEMANTIC_DISABLED: &str = "[search.semantic]\ndisabled = true\n";

/// A workspace fixture: one Rust source and a `rift.toml` that turns the semantic
/// tier off and whose `[server]` idle timeout reaps any orphaned server within a
/// minute.
fn workspace() -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        format!("{SEMANTIC_DISABLED}[server]\nidle_timeout = \"60s\"\n"),
    )?;
    Ok(directory)
}

/// Stops the fixture's server when a test unwinds, best effort.
struct StopOnDrop {
    root: PathBuf,
}

impl StopOnDrop {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
        }
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_rift"))
            .args(["server", "stop"])
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Runs the real binary with `arguments` inside the fixture workspace.
fn rift(root: &Path, arguments: &[&str]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_rift"))
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .output()?)
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn require_success(output: &Output, what: &str) -> TestResult {
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{what} must succeed: status {:?}, stdout {:?}, stderr {:?}",
        output.status,
        stdout_of(output),
        String::from_utf8_lossy(&output.stderr),
    )
    .into())
}

/// The `(port, pid)` a listening line names.
fn listening_facts(stdout: &str) -> TestResult<(u16, u32)> {
    let address = stdout
        .split_once("127.0.0.1:")
        .ok_or_else(|| format!("no listening address in {stdout:?}"))?
        .1;
    let port = address
        .split_once(' ')
        .ok_or_else(|| format!("no port terminator in {stdout:?}"))?
        .0
        .parse::<u16>()?;
    let pid = stdout
        .split_once("(pid ")
        .ok_or_else(|| format!("no pid in {stdout:?}"))?
        .1
        .split_once(')')
        .ok_or_else(|| format!("no pid terminator in {stdout:?}"))?
        .0
        .parse::<u32>()?;
    Ok((port, pid))
}

fn document_path(root: &Path) -> PathBuf {
    root.join(".rift").join(SERVER_LOCK_FILE_NAME)
}

/// Polls `condition` every [`POLL_INTERVAL`] up to `attempts` times.
fn wait_for<T>(
    attempts: u32,
    what: &str,
    mut condition: impl FnMut() -> Option<T>,
) -> TestResult<T> {
    for _ in 0..attempts {
        if let Some(value) = condition() {
            return Ok(value);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Err(format!("timed out waiting for {what}").into())
}

fn serving_document(root: &Path) -> Option<ServerLock> {
    match probe(root) {
        ServerPresence::Serving(lock) => Some(lock),
        ServerPresence::Stale(_) | ServerPresence::Absent => None,
    }
}

/// Waits until a fresh connect to `port` is refused again.
fn wait_until_port_refuses(port: u16) -> TestResult {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    wait_for(
        GONE_POLL_ATTEMPT_COUNT,
        "the released port to refuse",
        || {
            TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
                .is_err()
                .then_some(())
        },
    )
}

#[test]
fn start_serves_stop_shuts_down_and_both_repeat_idempotently() -> TestResult {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let started = rift(root, &["server", "start"])?;
    require_success(&started, "first start")?;
    let stdout = stdout_of(&started);
    assert!(
        stdout.contains("rift server listening on 127.0.0.1:"),
        "{stdout:?}"
    );
    let (port, pid) = listening_facts(&stdout)?;

    let document: ServerLock = serde_json::from_slice(&fs::read(document_path(root))?)?;
    document
        .validate()
        .map_err(|violation| format!("{violation:?}"))?;
    assert_eq!((document.port, document.pid), (port, pid));
    let serving = serving_document(root).ok_or("probe must report the started server")?;
    assert_eq!(serving, document);

    let repeated = rift(root, &["server", "start"])?;
    require_success(&repeated, "repeated start")?;
    let repeated_stdout = stdout_of(&repeated);
    assert!(
        repeated_stdout.contains("rift server already listening on 127.0.0.1:"),
        "{repeated_stdout:?}"
    );
    assert_eq!(listening_facts(&repeated_stdout)?, (port, pid));

    let stopped = rift(root, &["server", "stop"])?;
    require_success(&stopped, "stop")?;
    assert!(
        stdout_of(&stopped).contains("rift server stopped"),
        "{:?}",
        stdout_of(&stopped)
    );
    assert!(
        !document_path(root).exists(),
        "a graceful stop retires server.json"
    );
    assert!(
        serving_document(root).is_none(),
        "probe must not report a stopped server"
    );
    wait_until_port_refuses(port)?;

    let stopped_again = rift(root, &["server", "stop"])?;
    require_success(&stopped_again, "repeated stop")?;
    assert!(
        stdout_of(&stopped_again).contains("no rift server is running for this workspace"),
        "{:?}",
        stdout_of(&stopped_again)
    );
    Ok(())
}

#[test]
fn foreground_start_serves_until_stopped_and_exits_cleanly() -> TestResult {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let mut child = Command::new(env!("CARGO_BIN_EXE_rift"))
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let serving = wait_for(START_POLL_ATTEMPT_COUNT, "the foreground server", || {
        serving_document(root)
    })?;
    assert_eq!(serving.pid, child.id(), "the child itself must serve");

    let stopped = rift(root, &["server", "stop"])?;
    require_success(&stopped, "stop of a foreground server")?;

    wait_for(
        GONE_POLL_ATTEMPT_COUNT,
        "the foreground child to exit",
        || child.try_wait().ok().flatten(),
    )?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "a stopped foreground server exits cleanly: {:?}",
        output.status
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("rift server listening on 127.0.0.1:"),
        "the foreground server prints its listening line"
    );
    Ok(())
}

#[test]
fn concurrent_starts_agree_on_one_elected_server() -> TestResult {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let mut children = Vec::with_capacity(CONCURRENT_START_COUNT);
    for _ in 0..CONCURRENT_START_COUNT {
        children.push(
            Command::new(env!("CARGO_BIN_EXE_rift"))
                .args(["server", "start"])
                .current_dir(root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
    }
    let mut facts = Vec::with_capacity(CONCURRENT_START_COUNT);
    for child in children {
        let output = child.wait_with_output()?;
        require_success(&output, "concurrent start")?;
        facts.push(listening_facts(&stdout_of(&output))?);
    }
    let (port, pid) = facts[0];
    assert!(
        facts.iter().all(|&observed| observed == (port, pid)),
        "every start must name the same server: {facts:?}"
    );

    let document: ServerLock = serde_json::from_slice(&fs::read(document_path(root))?)?;
    assert_eq!((document.port, document.pid), (port, pid));
    let serving = serving_document(root).ok_or("the elected server must be live")?;
    assert_eq!(serving, document);

    let stopped = rift(root, &["server", "stop"])?;
    require_success(&stopped, "stop after the race")?;
    assert!(!document_path(root).exists());
    assert!(serving_document(root).is_none());
    Ok(())
}

#[test]
fn stale_document_is_replaced_by_a_fresh_election() -> TestResult {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    fs::create_dir_all(root.join(".rift"))?;
    let stale = ServerLock {
        port: 12_345,
        token: "a".repeat(SERVER_TOKEN_LENGTH),
        pid: 1,
        identity: stale_identity(),
    };
    fs::write(document_path(root), serde_json::to_vec(&stale)?)?;
    assert!(
        serving_document(root).is_none(),
        "a document without an election holder is not serving"
    );

    let started = rift(root, &["server", "start"])?;
    require_success(&started, "start over a stale document")?;
    let (_, pid) = listening_facts(&stdout_of(&started))?;
    assert_ne!(pid, 1, "a fresh server replaces the stale document");
    let document: ServerLock = serde_json::from_slice(&fs::read(document_path(root))?)?;
    assert_eq!(document.pid, pid);

    let stopped = rift(root, &["server", "stop"])?;
    require_success(&stopped, "stop after the stale replacement")?;
    assert!(!document_path(root).exists());
    Ok(())
}

#[test]
fn restart_replaces_the_serving_process() -> TestResult {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let started = rift(root, &["server", "start"])?;
    require_success(&started, "start before restart")?;
    let (_, first_pid) = listening_facts(&stdout_of(&started))?;

    // The restart's stop half polls until the first server released its
    // exclusive election lock, which the process holds for its whole life:
    // reaching the new listening line proves the old process stopped
    // serving. The new server may re-elect the same port, so port identity
    // is deliberately unasserted.
    let restarted = rift(root, &["server", "restart"])?;
    require_success(&restarted, "restart")?;
    let restarted_stdout = stdout_of(&restarted);
    assert!(
        restarted_stdout.contains("rift server listening on 127.0.0.1:"),
        "{restarted_stdout:?}"
    );
    let (_, second_pid) = listening_facts(&restarted_stdout)?;
    assert_ne!(second_pid, first_pid, "restart must elect a fresh process");
    let serving = serving_document(root).ok_or("the restarted server must be live")?;
    assert_eq!(serving.pid, second_pid);

    let stopped = rift(root, &["server", "stop"])?;
    require_success(&stopped, "stop after restart")?;
    assert!(serving_document(root).is_none());
    Ok(())
}

#[test]
fn stop_without_a_server_reports_and_discards_stale_state() -> TestResult {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = workspace()?;
    let root = directory.path();

    let idle = rift(root, &["server", "stop"])?;
    require_success(&idle, "stop without any lock state")?;
    assert!(
        stdout_of(&idle).contains("no rift server is running for this workspace"),
        "{:?}",
        stdout_of(&idle)
    );

    fs::create_dir_all(root.join(".rift"))?;
    fs::write(document_path(root), b"not json")?;
    let stale = rift(root, &["server", "stop"])?;
    require_success(&stale, "stop over a stale document")?;
    assert!(
        stdout_of(&stale).contains("no rift server is running for this workspace"),
        "{:?}",
        stdout_of(&stale)
    );
    assert!(
        !document_path(root).exists(),
        "a stale document is discarded best effort"
    );
    Ok(())
}

#[test]
fn status_reports_absent_stale_and_serving_states() -> TestResult {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);

    let absent = rift(root, &["server", "status"])?;
    require_success(&absent, "status without any lock state")?;
    assert!(
        stdout_of(&absent).contains("no rift server is running for this workspace"),
        "{:?}",
        stdout_of(&absent)
    );

    fs::create_dir_all(root.join(".rift"))?;
    let stale = ServerLock {
        port: 12_345,
        token: "a".repeat(SERVER_TOKEN_LENGTH),
        pid: 1,
        identity: stale_identity(),
    };
    fs::write(document_path(root), serde_json::to_vec(&stale)?)?;
    let stale_status = rift(root, &["server", "status"])?;
    require_success(&stale_status, "status over a stale document")?;
    let stale_stdout = stdout_of(&stale_status);
    assert!(
        stale_stdout.contains("found a stale .rift/server.json"),
        "{stale_stdout:?}"
    );
    assert!(
        stale_stdout.contains("no process holds the election lock"),
        "{stale_stdout:?}"
    );
    assert!(
        document_path(root).exists(),
        "status never discards the stale document"
    );

    let started = rift(root, &["server", "start"])?;
    require_success(&started, "start before the serving status")?;
    let (port, pid) = listening_facts(&stdout_of(&started))?;
    let document: ServerLock = serde_json::from_slice(&fs::read(document_path(root))?)?;
    let serving_status = rift(root, &["server", "status"])?;
    require_success(&serving_status, "status while serving")?;
    let serving_stdout = stdout_of(&serving_status);
    assert!(
        serving_stdout.contains(&format!(
            "rift server listening on 127.0.0.1:{port} (pid {pid}, v{version})",
            version = document.identity.version
        )),
        "{serving_stdout:?}"
    );

    let stopped = rift(root, &["server", "stop"])?;
    require_success(&stopped, "stop after the serving status")?;
    Ok(())
}
