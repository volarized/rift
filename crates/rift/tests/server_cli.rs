//! Real-binary contract of `rift server start|stop|restart|status`.
//!
//! Every test drives the compiled `rift` binary against a throwaway
//! workspace fixture, whose server binds a port the operating system assigned
//! rather than the first free port of the default range concurrent suites
//! share. The `election` nextest group admits one of these tests at a time.
//! Each fixture's `rift.toml` accepts a 60-second idle timeout as an
//! orphan-safety net, and a drop guard stops any server a failed test leaves
//! behind.

// The shared end-to-end harness brings its failure window; this suite reaches a subset.
#[expect(dead_code, reason = "shared end-to-end helper, used by sibling suites")]
mod engine_fixture;
#[expect(dead_code, reason = "shared end-to-end helper, used by sibling suites")]
mod harness;
#[expect(dead_code, reason = "shared end-to-end helper, used by sibling suites")]
mod rust_engine;

use std::error::Error;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rift_mcp::{START_POLL_ATTEMPT_COUNT, START_WAIT_MAX, ServerPresence, probe, stderr_file_path};
use rift_protocol::lock::{
    ProductIdentity, SERVER_LOCK_FILE_NAME, SERVER_PORT_MAX, SERVER_PORT_MIN, SERVER_TOKEN_LENGTH,
    ServerLock,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The executable supplied by the test runner, remapped when using an archive.
fn rift_binary() -> TestResult<PathBuf> {
    std::env::var_os("CARGO_BIN_EXE_rift")
        .map(PathBuf::from)
        .ok_or_else(|| "test runner must provide CARGO_BIN_EXE_rift".into())
}

/// Pause between polls of any awaited condition.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Poll attempts while waiting on a server to disappear, a child to exit,
/// or a port to refuse again: 10 seconds.
const GONE_POLL_ATTEMPT_COUNT: u32 = 100;
/// Bound on one probing TCP connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// Concurrent `rift server start` invocations in the election race.
const CONCURRENT_START_COUNT: usize = 4;
/// Source files in the workspace used by preparation and rewrite stop tests.
const LARGE_FIXTURE_FILES: usize = 3_000;
/// Documented declarations per file in the large workspace fixture.
const LARGE_FIXTURE_DECLARATIONS: usize = 12;
/// The startup stages recorded when a foreground server misses its start window.
const STARTUP_TRACE_FILTER: &str =
    "rift=info,rift_mcp=info,rift_server=info,rift_index=info,rift_mcp::validation=debug";
/// Test child variables that retain the server lifecycle lines this suite reads.
const SERVER_LOG_VARIABLES: [(&str, &str); 2] =
    [("RUST_LOG", STARTUP_TRACE_FILTER), ("NO_COLOR", "1")];
/// Bytes of a foreground server's standard error the test retains while it starts.
///
/// The reader drains every byte past this bound, so a server cannot wait on a full pipe while
/// its first index build runs. The retained output records the stage reached at a refusal.
const STARTUP_STDERR_BYTES_MAX: usize = 4 << 20;
/// How long the process may still run after `server.json` goes.
///
/// The serving process retires its own document, by dropping the election guard
/// immediately before it leaves, so a poll can land between the two. The stop bounds
/// that window; it does not remove it.
const DOCUMENT_GONE_GRACE: Duration = Duration::from_secs(2);
/// One foreground server stop, including observed process exit.
const DATABASE_REOPEN_STOP_BOUND: Duration = Duration::from_secs(5);
/// How long the server serves before the stop that tests the stop's own budget.
///
/// It outlasts `SERVER_STOP_DEADLINE`, the server-side stop's span, so a deadline
/// derived where the server began listening would already be spent when the stop lands.
const IDLE_SPAN_PAST_STOP_DEADLINE: Duration = Duration::from_secs(9);
/// How long a signalled foreground server has to leave: the five-second stop bound, which
/// the server's own four-second `SERVER_STOP_DEADLINE` sits inside.
#[cfg(unix)]
const STOP_EXIT_BOUND: Duration = Duration::from_secs(5);

fn stale_identity() -> ProductIdentity {
    ProductIdentity {
        version: "0.0.1".to_owned(),
        schema_digest: "b".repeat(64),
    }
}

/// The `[search.vector]` table every fixture here declares.
///
/// Rift ships the vector ranking on, so a fixture carrying no such table would acquire
/// the default model from the hub. A hermetic suite must not write into the developer's
/// own Hugging Face cache, and on a runner with no network a default-on tier would spend
/// its whole retry budget inside a detached task nobody waits on. `rift-mcp`'s
/// `tests/hermetic_search.rs` states the same policy for that crate's suites, and its
/// `live_vector_search` suite is the one place the shipped default is exercised.
const VECTOR_DISABLED: &str = "[search.vector]\ndisabled = true\n";

/// A workspace fixture: one Rust source and a `rift.toml` that turns the vector
/// ranking off, whose `[server]` idle timeout reaps any orphaned server within a
/// minute, and whose server binds an [`assigned_port`].
fn workspace() -> TestResult<tempfile::TempDir> {
    workspace_with_server_keys(&format!("port = {}\n", assigned_port()?))
}

/// The same fixture on the default serving range: a second server elected in the
/// workspace binds whether or not the first server's port is free yet.
fn workspace_on_the_default_range() -> TestResult<tempfile::TempDir> {
    workspace_with_server_keys("")
}

/// The fixture with `keys` added to its `[server]` table.
fn workspace_with_server_keys(keys: &str) -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    fs::write(
        directory.path().join("rift.toml"),
        format!("{VECTOR_DISABLED}[server]\nidle_timeout = \"60s\"\n{keys}"),
    )?;
    Ok(directory)
}

/// A loopback port the operating system assigned a moment ago and released.
///
/// Nextest runs each test in its own process, in parallel, and a server on the
/// default range binds its first free port, so servers from concurrent suites
/// hand ports between them; a fixture pins a port of its own instead.
fn assigned_port() -> TestResult<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Holds one port the server may select, so foreground serving exits at bind.
fn held_port_in_range() -> TestResult<TcpListener> {
    for port in SERVER_PORT_MIN..=SERVER_PORT_MAX {
        if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            return Ok(listener);
        }
    }
    Err("no free port in the serving range".into())
}

/// Fills the fixture with [`LARGE_FIXTURE_FILES`] Rust files under `src`, each declaring
/// [`LARGE_FIXTURE_DECLARATIONS`] documented functions.
fn write_large_fixture(root: &Path) -> TestResult {
    write_large_fixture_with_value(root, 0)
}

/// Writes a changed function value when the fixture is rewritten.
fn write_large_fixture_with_value(root: &Path, value: u64) -> TestResult {
    use std::fmt::Write as _;

    let source = root.join("src");
    fs::create_dir_all(&source)?;
    for file in 0..LARGE_FIXTURE_FILES {
        let mut contents = String::new();
        for declaration in 0..LARGE_FIXTURE_DECLARATIONS {
            writeln!(
                contents,
                "/// Beacon {declaration} in file {file}.\npub fn beacon_{file}_{declaration}(value: \
                 u64) -> u64 {{\n    value + {declaration} + {value}\n}}\n"
            )?;
        }
        fs::write(source.join(format!("file_{file}.rs")), contents)?;
    }
    Ok(())
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
        let Ok(binary) = rift_binary() else {
            return;
        };
        let _ = Command::new(binary)
            .args(["server", "stop"])
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// A foreground server's standard error, drained while the server starts.
struct StderrWatch {
    bytes: Arc<Mutex<Vec<u8>>>,
    reader: std::thread::JoinHandle<()>,
}

impl StderrWatch {
    /// Drains `stream` on another thread so the foreground server can keep writing startup
    /// records. The retained prefix stays bounded for the failure message.
    fn spawn(mut stream: ChildStderr) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&bytes);
        let reader = std::thread::spawn(move || {
            let mut buffer = [0_u8; 8 << 10];
            loop {
                let count = match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                let mut retained = captured
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let room = STARTUP_STDERR_BYTES_MAX.saturating_sub(retained.len());
                retained.extend_from_slice(&buffer[..count.min(room)]);
            }
        });
        Self { bytes, reader }
    }

    /// Standard error retained so far, for a start refusal before the child ends.
    fn snapshot(&self) -> String {
        let retained = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&retained).into_owned()
    }

    /// The last bytes of standard error retained once the child's stream closed, for a
    /// failure after the child ended, bounded by [`harness::bounded_tail`].
    ///
    /// The wait for the reader is bounded, so a stream another process still holds open
    /// delays the failure message and never the test's own deadline.
    fn after_exit(&self) -> String {
        let _ = wait_for(GONE_POLL_ATTEMPT_COUNT, "the stderr reader to end", || {
            self.reader.is_finished().then_some(())
        });
        harness::bounded_tail(&self.snapshot())
    }

    /// Waits for the reader once the foreground child ended.
    fn finished(self) -> TestResult<String> {
        let Self { bytes, reader } = self;
        reader
            .join()
            .map_err(|_panic| "the foreground server stderr reader panicked")?;
        let mut retained = bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bytes = std::mem::take(&mut *retained);
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Reads a foreground child's validated document after it publishes.
///
/// A foreground child claims the election before it binds and publishes. This read leaves that
/// claim alone while the test waits; the document is written only after the transport binds.
fn published_foreground_document(root: &Path, child_pid: u32) -> Option<ServerLock> {
    let bytes = fs::read(document_path(root)).ok()?;
    let lock: ServerLock = serde_json::from_slice(&bytes).ok()?;
    lock.validate().ok()?;
    (lock.pid == child_pid).then_some(lock)
}

/// Waits for the foreground server's document and records what its child reached on failure.
fn wait_for_foreground_server(
    root: &Path,
    child: &mut Child,
    stderr: &StderrWatch,
) -> TestResult<ServerLock> {
    match wait_for(START_POLL_ATTEMPT_COUNT, "the foreground server", || {
        published_foreground_document(root, child.id())
    }) {
        Ok(serving) => Ok(serving),
        Err(error) => {
            let status = child.try_wait()?;
            Err(format!(
                "{error}; foreground server status: {status:?}; stderr: {}",
                harness::bounded_tail(&stderr.snapshot())
            )
            .into())
        }
    }
}

/// Runs the real binary with `arguments` inside the fixture workspace, under
/// [`SERVER_LOG_VARIABLES`]; a started server inherits them too.
fn rift(root: &Path, arguments: &[&str]) -> TestResult<Output> {
    rift_with_variables(root, arguments, &[])
}

/// Runs the CLI with [`SERVER_LOG_VARIABLES`], then `variables`, added to the inherited
/// environment; a started server inherits them too.
fn rift_with_variables(
    root: &Path,
    arguments: &[&str],
    variables: &[(&str, &str)],
) -> TestResult<Output> {
    Ok(Command::new(rift_binary()?)
        .args(arguments)
        .envs(SERVER_LOG_VARIABLES)
        .envs(variables.iter().copied())
        .current_dir(root)
        .stdin(Stdio::null())
        .output()?)
}

/// Parenthesis nesting past the default `[providers.syntax] max_depth` of 512.
const DEEP_NESTING: usize = 600;

/// One Rust file whose expression nests past the default syntax depth bound.
fn deep_source() -> String {
    format!(
        "pub fn deep() -> i32 {{ {open}1{close} }}\n",
        open = "(".repeat(DEEP_NESTING),
        close = ")".repeat(DEEP_NESTING),
    )
}

/// Starts the workspace's server with `variables`, then polls `search` for
/// `query` until the answer holds `expected`.
fn started_answer_holding(
    root: &Path,
    variables: &[(&str, &str)],
    query: &str,
    expected: &str,
) -> TestResult<String> {
    let started = rift_with_variables(root, &["server", "start"], variables)?;
    require_success(&started, "start with variables")?;
    let serving = serving_document(root).ok_or("probe must report the started server")?;
    wait_for(START_POLL_ATTEMPT_COUNT, expected, || {
        search_request(serving.port, &serving.token, query)
            .ok()
            .filter(|answer| answer.contains(expected))
    })
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

/// Polls `condition` every [`POLL_INTERVAL`] up to `attempts` times, and for no
/// longer than those attempts span at that interval.
///
/// A condition that probes the workspace is not instant: a probe of a port
/// nothing accepts on spends its whole connect timeout, which on Windows is
/// every refused port, so counting alone would stretch the wait several times
/// over.
fn wait_for<T>(
    attempts: u32,
    what: &str,
    mut condition: impl FnMut() -> Option<T>,
) -> TestResult<T> {
    let deadline = std::time::Instant::now() + POLL_INTERVAL.saturating_mul(attempts);
    for _ in 0..attempts {
        if let Some(value) = condition() {
            return Ok(value);
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Err(format!("timed out waiting for {what}").into())
}

/// The fields the server's own stop line carries, as its stderr rendered them:
/// `key=value` pairs after the message, string values without quotes.
fn stop_line_of(stderr: &str) -> TestResult<&str> {
    let after_message = stderr
        .split_once("MCP server stopped")
        .ok_or_else(|| format!("serving must end before the process leaves: {stderr:?}"))?
        .1;
    Ok(after_message.lines().next().unwrap_or_default())
}

fn serving_document(root: &Path) -> Option<ServerLock> {
    match probe(root) {
        ServerPresence::Serving(lock) => Some(lock),
        ServerPresence::Starting | ServerPresence::Stale(_) | ServerPresence::Absent => None,
    }
}

/// Calls the `search` tool over the served MCP path and reads the whole answer.
///
/// The server serves MCP statelessly with JSON responses, so one authorized
/// `POST` carries the whole call and the reply arrives on the same connection,
/// which `Connection: close` ends. A query matching every unit of the large
/// fixture keeps the request in flight while the stop lands.
fn search_request(port: u16, token: &str, query: &str) -> TestResult<String> {
    search_request_with_timeout(port, token, query, None)
}

fn search_request_with_timeout(
    port: u16,
    token: &str,
    query: &str,
    timeout: Option<Duration>,
) -> TestResult<String> {
    let body = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "search", "arguments": {"query": query}},
    }))?;
    let head = format!(
        "POST /api/mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n\
         Content-Length: {length}\r\nConnection: close\r\n\r\n",
        length = body.len()
    );
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
    if let Some(timeout) = timeout {
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
    }
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer)?;
    Ok(answer)
}

fn symbol_request(port: u16, token: &str, name: &str) -> TestResult<String> {
    mcp_request(
        port,
        token,
        "tools/call",
        &serde_json::json!({"name": "get_symbol", "arguments": {"name": name}}),
    )
}

/// One authorized JSON-RPC `method` over the served MCP path, as the raw HTTP answer.
fn mcp_request(
    port: u16,
    token: &str,
    method: &str,
    params: &serde_json::Value,
) -> TestResult<String> {
    let body = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))?;
    let head = format!(
        "POST /api/mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\n\
         Content-Length: {length}\r\nConnection: close\r\n\r\n",
        length = body.len()
    );
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer)?;
    Ok(answer)
}

/// The JSON body of one raw HTTP answer.
fn answer_body(answer: &str) -> TestResult<serde_json::Value> {
    let body = answer
        .split_once("\r\n\r\n")
        .ok_or("the MCP response must contain an HTTP body")?
        .1;
    Ok(serde_json::from_str(body)?)
}

fn lexical_content_hit(answer: &str, path: &str) -> TestResult<serde_json::Value> {
    let body = answer
        .split_once("\r\n\r\n")
        .ok_or("the MCP response must contain an HTTP body")?
        .1;
    let response: serde_json::Value = serde_json::from_str(body)?;
    let results = response["result"]["structuredContent"]["results"]
        .as_array()
        .ok_or("the search response must contain structured results")?;
    results
        .iter()
        .find(|hit| {
            hit["path"] == path
                && hit["matched_by"]
                    .as_array()
                    .is_some_and(|fields| fields.iter().any(|field| field == "content"))
        })
        .cloned()
        .ok_or_else(|| {
            format!("search must return {path} through lexical content: {answer}").into()
        })
}

fn wait_for_lexical_content_hit(
    port: u16,
    token: &str,
    query: &str,
    path: &str,
) -> TestResult<serde_json::Value> {
    let deadline =
        std::time::Instant::now() + POLL_INTERVAL.saturating_mul(START_POLL_ATTEMPT_COUNT);
    let mut last_response = String::from("no response received");
    for _ in 0..START_POLL_ATTEMPT_COUNT {
        match search_request_with_timeout(port, token, query, Some(CONNECT_TIMEOUT)) {
            Ok(answer) => match lexical_content_hit(&answer, path) {
                Ok(hit) => return Ok(hit),
                Err(error) => last_response = error.to_string(),
            },
            Err(error) => last_response = error.to_string(),
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Err(format!("timed out waiting for the lexical content hit: {last_response}").into())
}

/// Starts a foreground server under [`SERVER_LOG_VARIABLES`] and returns once it
/// published its document, with the watch on its standard error that every stop of it
/// passes to [`stop_foreground_server`].
fn start_foreground_server(root: &Path) -> TestResult<(Child, ServerLock, StderrWatch)> {
    let mut child = Command::new(rift_binary()?)
        .args(["server", "start", "--foreground"])
        .envs(SERVER_LOG_VARIABLES)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(child.stderr.take().ok_or("the child's stderr is piped")?);
    match wait_for_foreground_server(root, &mut child, &stderr) {
        Ok(serving) => Ok((child, serving, stderr)),
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

/// Stops the foreground server and requires a clean exit inside the stop bound.
///
/// A failure after the stop request carries the server's exit status and the last bytes
/// the server wrote to its standard error, which `stderr` watches: the rendered error
/// behind a failed status and the record of each stop stage, including the stages after
/// the log store closed, which reach stderr alone.
fn stop_foreground_server(root: &Path, child: &mut Child, stderr: &StderrWatch) -> TestResult {
    let started = std::time::Instant::now();
    let deadline = started + DATABASE_REOPEN_STOP_BOUND;
    let mut stop = Command::new(rift_binary()?)
        .args(["server", "stop"])
        .envs(SERVER_LOG_VARIABLES)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    loop {
        let stop_status = stop.try_wait()?;
        let server_status = child.try_wait()?;
        if let (Some(stop_status), Some(server_status)) = (stop_status, server_status) {
            let stop_output = stop.wait_with_output()?;
            require_success(&stop_output, "stop the foreground server")?;
            assert!(
                stop_status.success(),
                "stop command must exit cleanly: {stop_status:?}"
            );
            assert!(
                server_status.success(),
                "foreground server must exit cleanly: {server_status:?}; stderr: {}",
                stderr.after_exit()
            );
            assert!(
                started.elapsed() <= DATABASE_REOPEN_STOP_BOUND,
                "stop and observed process exit must fit {DATABASE_REOPEN_STOP_BOUND:?}"
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let _ = stop.kill();
            let _ = stop.wait();
            let _ = child.kill();
            let server_status = child.wait()?;
            return Err(format!(
                "stop and process exit exceeded {DATABASE_REOPEN_STOP_BOUND:?}; server status: \
                 {server_status:?}; stderr: {}",
                stderr.after_exit()
            )
            .into());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Whether one `rift server stop` printed the line that reports success.
fn reports_stopped(output: &Output) -> bool {
    stdout_of(output).contains("rift server stopped")
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
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let started = rift_with_variables(root, &["server", "start"], &SERVER_LOG_VARIABLES)?;
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

    // The detached server's stderr lands in the workspace file the start
    // truncated for it, where its own startup lines are the first content.
    let stderr = fs::read_to_string(stderr_file_path(root))?;
    assert!(
        stderr.contains("MCP server ready"),
        "the detached server's stderr file carries its startup line: {stderr:?}"
    );

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
    failure_window.passed();
    Ok(())
}

#[test]
fn direct_http_stays_all_with_text_content_structured_content_and_output_schemas() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let started = rift(root, &["server", "start"])?;
    require_success(&started, "start before the direct HTTP calls")?;
    let serving = serving_document(root).ok_or("the started server must publish its document")?;

    let listed = answer_body(&mcp_request(
        serving.port,
        &serving.token,
        "tools/list",
        &serde_json::json!({}),
    )?)?;
    let tools = listed["result"]["tools"]
        .as_array()
        .ok_or("tools/list must carry a tools array")?;
    assert!(!tools.is_empty(), "{listed}");
    assert!(
        tools.iter().all(|tool| tool["outputSchema"].is_object()),
        "direct HTTP must list the output schema of every tool: {listed}"
    );

    let result = wait_for(START_POLL_ATTEMPT_COUNT, "a completed symbol read", || {
        let answer = symbol_request(serving.port, &serving.token, "beacon").ok()?;
        let result = answer_body(&answer).ok()?["result"].clone();
        result["structuredContent"].is_object().then_some(result)
    })?;
    let text = result["content"][0]["text"].as_str();
    assert!(
        text.is_some_and(|text| !text.is_empty()),
        "direct HTTP must return a non-empty text block: {result}"
    );
    failure_window.passed();
    Ok(())
}

#[test]
fn a_stopped_server_reopens_its_database_for_search() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    fs::write(
        root.join("notes.md"),
        "amber canoe velvet: persistent lexical content\n",
    )?;
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let (mut child, serving, stderr) = start_foreground_server(root)?;
    let first = wait_for_lexical_content_hit(
        serving.port,
        &serving.token,
        "amber canoe velvet",
        "notes.md",
    )?;

    stop_foreground_server(root, &mut child, &stderr)?;
    wait_until_port_refuses(serving.port)?;

    let (mut child, reopened, stderr) = start_foreground_server(root)?;
    assert_ne!(reopened.pid, serving.pid, "reopen must elect a new process");
    let second = wait_for_lexical_content_hit(
        reopened.port,
        &reopened.token,
        "amber canoe velvet",
        "notes.md",
    )?;
    assert_eq!(second, first, "reopen must preserve the lexical search hit");

    stop_foreground_server(root, &mut child, &stderr)?;
    wait_until_port_refuses(reopened.port)?;
    failure_window.passed();
    Ok(())
}

#[test]
fn foreground_start_serves_until_stopped_and_exits_cleanly() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let mut child = Command::new(rift_binary()?)
        .args(["server", "start", "--foreground"])
        .envs(SERVER_LOG_VARIABLES)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(child.stderr.take().ok_or("the child's stderr is piped")?);
    let serving = wait_for_foreground_server(root, &mut child, &stderr)?;
    assert_eq!(serving.pid, child.id(), "the child itself must serve");

    // The stop requires the clean exit; the listening line stays in the piped stdout.
    stop_foreground_server(root, &mut child, &stderr)?;
    let output = child.wait_with_output()?;
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("rift server listening on 127.0.0.1:"),
        "the foreground server prints its listening line"
    );
    failure_window.passed();
    Ok(())
}

/// A foreground server that has printed its listening line, with the stdout it prints on.
///
/// The reader stays open until the process leaves: the server prints its outcome line as
/// it exits, and a closed stdout would fail that write.
#[cfg(unix)]
struct ListeningForeground {
    child: std::process::Child,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

#[cfg(unix)]
impl ListeningForeground {
    /// Starts a foreground server in `root` with `variables` added to the inherited
    /// environment, and returns once it has printed its listening line: the server installs
    /// its stop signal handlers before it prints that line.
    fn start(root: &Path, variables: &[(&str, &str)]) -> TestResult<Self> {
        use std::io::BufRead as _;

        let mut child = Command::new(rift_binary()?)
            .args(["server", "start", "--foreground"])
            .envs(SERVER_LOG_VARIABLES)
            .envs(variables.iter().copied())
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let serving = wait_for(START_POLL_ATTEMPT_COUNT, "the foreground server", || {
            published_foreground_document(root, child.id())
        })?;
        assert_eq!(serving.pid, child.id(), "the child itself must serve");
        let stdout = child
            .stdout
            .take()
            .ok_or("the foreground server's stdout is piped")?;
        let mut stdout = std::io::BufReader::new(stdout);
        let mut listening = String::new();
        stdout.read_line(&mut listening)?;
        assert!(
            listening.contains("rift server listening on 127.0.0.1:"),
            "the foreground server prints its listening line first: {listening:?}"
        );
        Ok(Self { child, stdout })
    }

    /// Sends SIGTERM, waits for the process to leave, and returns its status, the time it
    /// took, and everything it wrote to stderr.
    fn terminate(mut self) -> TestResult<(std::process::ExitStatus, Duration, String)> {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;

        let pid = Pid::from_raw(i32::try_from(self.child.id())?);
        let signalled = std::time::Instant::now();
        kill(pid, Signal::SIGTERM)?;
        let status = wait_for(
            GONE_POLL_ATTEMPT_COUNT,
            "the signalled foreground server to exit",
            || self.child.try_wait().ok().flatten(),
        )?;
        let elapsed = signalled.elapsed();
        let mut stdout = String::new();
        self.stdout.read_to_string(&mut stdout)?;
        let output = self.child.wait_with_output()?;
        Ok((
            status,
            elapsed,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

#[cfg(unix)]
#[test]
fn sigterm_stops_a_foreground_server_through_its_stop() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let server = ListeningForeground::start(root, &[])?;
    let (status, elapsed, stderr) = server.terminate()?;

    assert!(
        status.success(),
        "SIGTERM runs the stop, and the process exits cleanly: {status:?}"
    );
    assert!(
        elapsed <= STOP_EXIT_BOUND,
        "a signalled server exits inside the stop bound: elapsed={elapsed:?}, bound={STOP_EXIT_BOUND:?}"
    );
    let stop_line = stop_line_of(&stderr)?;
    assert!(
        stop_line
            .split_whitespace()
            .any(|field| field == "outcome=ok"),
        "the engines and the index supervisor join inside the budget: {stop_line}"
    );
    assert!(
        !document_path(root).exists(),
        "the stop retires server.json"
    );
    failure_window.passed();
    Ok(())
}

/// Every OTLP/HTTP export request one receiver answered, with when it arrived.
#[cfg(all(unix, feature = "otlp"))]
type ReceivedExports = std::sync::Arc<std::sync::Mutex<Vec<(std::time::Instant, usize)>>>;

/// An OTLP/HTTP receiver on a loopback port that records each export and answers success.
#[cfg(all(unix, feature = "otlp"))]
struct TraceReceiver {
    _runtime: tokio::runtime::Runtime,
    port: u16,
    exports: ReceivedExports,
}

#[cfg(all(unix, feature = "otlp"))]
impl TraceReceiver {
    fn start() -> TestResult<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let listener = runtime.block_on(tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))?;
        let port = listener.local_addr()?.port();
        let exports = ReceivedExports::default();
        let recorded = std::sync::Arc::clone(&exports);
        let receiver = axum::Router::new().route(
            "/v1/traces",
            axum::routing::post(move |body: axum::body::Bytes| async move {
                recorded
                    .lock()
                    .expect("the recorded exports are not poisoned")
                    .push((std::time::Instant::now(), body.len()));
                axum::http::StatusCode::OK
            }),
        );
        runtime.spawn(async move { axum::serve(listener, receiver).await });
        Ok(Self {
            _runtime: runtime,
            port,
            exports,
        })
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The byte counts of the exports that arrived at or after `moment`.
    fn exports_since(&self, moment: std::time::Instant) -> Vec<usize> {
        self.exports
            .lock()
            .expect("the recorded exports are not poisoned")
            .iter()
            .filter(|(arrived, _)| *arrived >= moment)
            .map(|(_, bytes)| *bytes)
            .collect()
    }
}

/// The batch processor's export interval the flush test sets: ten minutes, so no scheduled
/// export runs while the server serves and only the shutdown flush sends its spans.
#[cfg(all(unix, feature = "otlp"))]
const EXPORT_INTERVAL_PAST_THE_TEST_MS: &str = "600000";

#[cfg(all(unix, feature = "otlp"))]
#[test]
fn sigterm_flushes_the_otlp_export_before_the_process_exits() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);
    let receiver = TraceReceiver::start()?;
    let endpoint = receiver.endpoint();

    let server = ListeningForeground::start(
        root,
        &[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint.as_str()),
            ("OTEL_BSP_SCHEDULE_DELAY", EXPORT_INTERVAL_PAST_THE_TEST_MS),
        ],
    )?;
    let signalled = std::time::Instant::now();
    let (status, elapsed, stderr) = server.terminate()?;

    assert!(
        status.success(),
        "SIGTERM runs the stop, and the process exits cleanly: {status:?}, stderr: {stderr}"
    );
    assert!(
        elapsed <= STOP_EXIT_BOUND,
        "a signalled server flushes and exits inside the stop bound: elapsed={elapsed:?}, bound={STOP_EXIT_BOUND:?}"
    );
    let flushed = receiver.exports_since(signalled);
    assert!(
        flushed.iter().any(|bytes| *bytes > 0),
        "the export shutdown sends the spans the server closed while serving: {flushed:?}"
    );
    failure_window.passed();
    Ok(())
}

// Native startup regression: https://github.com/volarized/rift/issues/447
#[test]
fn a_stop_after_a_long_serving_span_still_runs_every_stage_inside_its_budget() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let mut child = Command::new(rift_binary()?)
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .envs(SERVER_LOG_VARIABLES)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(child.stderr.take().ok_or("the child's stderr is piped")?);
    let serving = wait_for_foreground_server(root, &mut child, &stderr)?;
    assert_eq!(serving.pid, child.id(), "the child itself must serve");

    // The server serves past its own stop span before anyone asks it to stop, so a
    // deadline derived at startup would leave every later stage nothing to spend.
    std::thread::sleep(IDLE_SPAN_PAST_STOP_DEADLINE);
    stop_foreground_server(root, &mut child, &stderr)?;

    wait_for(
        GONE_POLL_ATTEMPT_COUNT,
        "the foreground child to exit",
        || child.try_wait().ok().flatten(),
    )?;
    let status = child.wait()?;
    assert!(
        status.success(),
        "a stopped foreground server exits cleanly: {status:?}"
    );
    let stderr = stderr.finished()?;
    let stop_line = stop_line_of(&stderr)?;
    assert!(
        stop_line
            .split_whitespace()
            .any(|field| field == "outcome=ok"),
        "the engines and the index supervisor must join inside the budget: {stop_line}"
    );
    assert!(
        !stderr.contains("outlasted the stop deadline"),
        "the log drain's final flush must get its share of the budget: {stderr}"
    );
    failure_window.passed();
    Ok(())
}

// Native startup regression: https://github.com/volarized/rift/issues/447
#[test]
fn stop_after_large_workspace_binds_ends_the_process() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    write_large_fixture(root)?;
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let mut child = Command::new(rift_binary()?)
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .envs(SERVER_LOG_VARIABLES)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(child.stderr.take().ok_or("the child's stderr is piped")?);
    let serving = wait_for_foreground_server(root, &mut child, &stderr)?;
    assert_eq!(serving.pid, child.id(), "the child itself must serve");

    // The document appears after HTTP binds, while source preparation can still run.
    // The lane's injected cancellation tests separately prove held transaction abort.
    stop_foreground_server(root, &mut child, &stderr)?;

    // The stop helper already observes CLI completion and process exit within five seconds.
    let status = wait_for(
        GONE_POLL_ATTEMPT_COUNT,
        "the stopped server's process to exit after the listener binds",
        || child.try_wait().ok().flatten(),
    )?;
    assert!(
        status.success(),
        "a stopped foreground server exits cleanly: {status:?}"
    );
    child.wait()?;
    let stderr = stderr.finished()?;
    assert!(
        stderr.contains("MCP server stopped"),
        "serving ended before the process left: {stderr}"
    );
    assert!(
        !document_path(root).exists(),
        "a graceful stop retires server.json"
    );
    failure_window.passed();
    Ok(())
}

// Native startup regression: https://github.com/volarized/rift/issues/447
#[test]
fn stop_after_large_fixture_rewrite_ends_the_process() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    write_large_fixture(root)?;
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let mut child = Command::new(rift_binary()?)
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .envs(SERVER_LOG_VARIABLES)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(child.stderr.take().ok_or("the child's stderr is piped")?);
    let serving = wait_for_foreground_server(root, &mut child, &stderr)?;
    assert_eq!(serving.pid, child.id(), "the child itself must serve");

    // A published document identifies the listener. Changed source bytes then feed
    // invalidations during preparation or a later rebuild. Injected supervisor tests
    // separately prove cancellation while a capture remains held.
    write_large_fixture_with_value(root, 1)?;
    stop_foreground_server(root, &mut child, &stderr)?;

    // The stop helper already observes CLI completion and process exit within five seconds.
    let status = wait_for(
        GONE_POLL_ATTEMPT_COUNT,
        "the stopped server's process to exit after the source rewrite",
        || child.try_wait().ok().flatten(),
    )?;
    assert!(
        status.success(),
        "a stopped foreground server exits cleanly: {status:?}"
    );
    child.wait()?;
    let stderr = stderr.finished()?;
    assert!(
        stderr.contains("MCP server stopped"),
        "serving ended before the process left: {stderr}"
    );
    assert!(
        !document_path(root).exists(),
        "a graceful stop retires server.json"
    );
    failure_window.passed();
    Ok(())
}

// Native startup regression: https://github.com/volarized/rift/issues/447
#[test]
fn stop_after_large_fixture_rewrite_ends_the_process_as_the_document_goes() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    write_large_fixture(root)?;
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let mut child = Command::new(rift_binary()?)
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .env("RUST_LOG", STARTUP_TRACE_FILTER)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(child.stderr.take().ok_or("the child's stderr is piped")?);
    let serving = wait_for_foreground_server(root, &mut child, &stderr)?;
    assert_eq!(serving.pid, child.id(), "the child itself must serve");

    // Every rewrite changes bytes. The watcher owes validation during preparation or
    // a later rebuild; the document alone does not identify a capture or transaction.
    write_large_fixture_with_value(root, 1)?;
    let stop_started = std::time::Instant::now();
    let stopped = rift(root, &["server", "stop"])?;
    require_success(&stopped, "stop after the source rewrite")?;

    // Each poll reads the document before the process, so a poll never manufactures the
    // order it measures.
    let mut observations = Vec::with_capacity(GONE_POLL_ATTEMPT_COUNT as usize);
    let status = wait_for(
        GONE_POLL_ATTEMPT_COUNT,
        "the stopped server's process to exit after the source rewrite",
        || {
            let document_present = document_path(root).exists();
            let exited = child.try_wait().ok().flatten();
            observations.push((document_present, exited.is_some()));
            exited
        },
    )?;
    assert!(
        status.success(),
        "a stopped foreground server exits cleanly: {status:?}; stderr: {}",
        stderr.after_exit()
    );
    assert!(
        stop_started.elapsed() <= DATABASE_REOPEN_STOP_BOUND,
        "stop and observed process exit must fit {DATABASE_REOPEN_STOP_BOUND:?}"
    );
    let lingering_polls = observations
        .iter()
        .filter(|&&(document_present, exited)| !document_present && !exited)
        .count();
    let lingering = POLL_INTERVAL * u32::try_from(lingering_polls).unwrap_or(u32::MAX);
    assert!(
        lingering <= DOCUMENT_GONE_GRACE,
        "the process leaves within {DOCUMENT_GONE_GRACE:?} of server.json going, observed \
         {lingering:?}; (document present, process exited) per poll: {observations:?}"
    );
    child.wait()?;
    let stderr = stderr.finished()?;
    assert!(
        stderr.contains("MCP server stopped"),
        "serving ended before the process left: {stderr}"
    );
    assert!(
        !document_path(root).exists(),
        "a graceful stop retires server.json"
    );
    failure_window.passed();
    Ok(())
}

// Native startup regression: https://github.com/volarized/rift/issues/447
#[test]
fn a_stop_reports_success_only_once_the_election_it_waited_on_released() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    write_large_fixture(root)?;
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let mut child = Command::new(rift_binary()?)
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .env("RUST_LOG", STARTUP_TRACE_FILTER)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(child.stderr.take().ok_or("the child's stderr is piped")?);
    let serving = wait_for_foreground_server(root, &mut child, &stderr)?;
    assert_eq!(serving.pid, child.id(), "the child itself must serve");

    // Search and stop run concurrently. Search may answer during preparation, so this
    // case observes election release without requiring a held search request.
    let searching = std::thread::spawn(move || {
        let _answer = search_request(serving.port, &serving.token, "beacon");
    });
    let stop_started = std::time::Instant::now();
    let stopping = std::thread::spawn({
        let root = root.to_owned();
        move || rift(&root, &["server", "stop"]).map_err(|error| error.to_string())
    });

    // Every stop asked while the first one waits meets the workspace mid-shutdown.
    // Each answer is read with the election state it was printed under. Only the
    // stop the server answers is required to report success: a sibling's request can
    // be cut by the cancellation the answered one triggered, and that stop refuses
    // with `server_stop_failed` rather than claiming a stop it never delivered.
    let mut observations = Vec::new();
    while !stopping.is_finished() {
        let again = rift(root, &["server", "stop"])?;
        observations.push((reports_stopped(&again), probe(root).election_held()));
    }
    let stopped = stopping
        .join()
        .map_err(|_| "the stop thread must not panic")??;
    observations.push((reports_stopped(&stopped), probe(root).election_held()));
    assert!(
        observations
            .iter()
            .any(|&(reported_stopped, _)| reported_stopped),
        "the stop the server answered must report success; observed (reported \
         stopped, election held) per stop: {observations:?}"
    );
    assert!(
        observations
            .iter()
            .all(|&(reported_stopped, election_held)| !reported_stopped || !election_held),
        "a stop reports success only once the election released; observed (reported \
         stopped, election held) per stop: {observations:?}"
    );

    // The election releases immediately before the process leaves, so a reported stop
    // is followed by the exit itself. The exit status is left unasserted: it carries
    // whether the drain finished inside the stop's budget, which this fixture's search
    // does not pin.
    wait_for(
        GONE_POLL_ATTEMPT_COUNT,
        "the stopped server's process to exit after its stop reported success",
        || child.try_wait().ok().flatten(),
    )?;
    child.wait()?;
    assert!(
        stop_started.elapsed() <= DATABASE_REOPEN_STOP_BOUND,
        "stop and observed process exit must fit {DATABASE_REOPEN_STOP_BOUND:?}"
    );
    let stderr = stderr.finished()?;
    assert!(
        stderr.contains("MCP server stopped"),
        "serving ended before the process left: {stderr}"
    );
    assert!(
        !document_path(root).exists(),
        "a graceful stop retires server.json"
    );
    let _ = searching.join();
    failure_window.passed();
    Ok(())
}

#[test]
fn concurrent_starts_agree_on_one_elected_server() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let mut children = Vec::with_capacity(CONCURRENT_START_COUNT);
    for _ in 0..CONCURRENT_START_COUNT {
        children.push(
            Command::new(rift_binary()?)
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
    failure_window.passed();
    Ok(())
}

/// A refused election names its outcome and the holder's process once `server.json`
/// publishes it: a foreground start beside a published server leaves with
/// `server_already_serving`, and its refusal names the holder's pid and address.
/// Replays the refused election of the tracing plan's failure list.
#[test]
fn a_refused_foreground_start_names_the_published_holder() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);
    let started = rift(root, &["server", "start"])?;
    require_success(&started, "the published server's start")?;
    let (port, pid) = listening_facts(&stdout_of(&started))?;

    let refused = rift(root, &["server", "start", "--foreground"])?;

    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success(),
        "the published server holds the election: {}",
        harness::bounded_tail(&stderr)
    );
    for named in [
        "server_already_serving".to_owned(),
        format!("listening 127.0.0.1:{port}"),
        format!("pid {pid}"),
    ] {
        assert!(
            stderr.contains(&named),
            "the refusal names {named:?}: {}",
            harness::bounded_tail(&stderr)
        );
    }
    let serving = serving_document(root).ok_or("the published server keeps serving")?;
    assert_eq!(serving.pid, pid);
    failure_window.passed();
    Ok(())
}

/// Prints the failure window of every compiled-binary test whose process ended before its
/// own window printed - a nextest timeout or another kill - and fails when it printed one.
///
/// Run it after the nextest run, alone, so it prints no window of a test still running:
/// `cargo nextest run -p rift --test server_cli --run-ignored only --no-tests fail -E
/// 'test(=failure_windows_of_ended_tests)'`.
#[test]
#[ignore = "run after a nextest run, to print the windows of the tests it ended"]
fn failure_windows_of_ended_tests() -> TestResult {
    let printed = harness::print_ended_windows()?;
    if printed > 0 {
        return Err(format!(
            "{printed} tests ended before their failure window printed; the windows are above"
        )
        .into());
    }
    Ok(())
}

#[test]
fn stale_document_is_replaced_by_a_fresh_election() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    fs::create_dir_all(root.join(".rift"))?;
    let stale = ServerLock {
        port: 12_345,
        token: "a".repeat(SERVER_TOKEN_LENGTH),
        pid: 1,
        identity: stale_identity(),
        server: None,
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
    failure_window.passed();
    Ok(())
}

#[test]
fn restart_replaces_the_serving_process() -> TestResult {
    let directory = workspace_on_the_default_range()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

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
    failure_window.passed();
    Ok(())
}

#[test]
fn stop_without_a_server_reports_and_discards_stale_state() -> TestResult {
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

/// A server that fails at startup exits before publishing: the start reports the
/// exited child's pid and points at the stderr file that holds the refusal.
#[test]
fn start_reports_a_server_that_exits_before_publishing() -> TestResult {
    let held = held_port_in_range()?;
    let directory = workspace_with_server_keys(&format!("port = {}\n", held.local_addr()?.port()))?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let started = rift(root, &["server", "start"])?;
    let stderr = String::from_utf8_lossy(&started.stderr);
    assert!(
        !started.status.success(),
        "a server that exits before publishing fails the start: {:?}",
        stdout_of(&started)
    );
    assert!(stderr.contains("server_start_failed"), "{stderr:?}");
    assert!(stderr.contains("exited before publishing"), "{stderr:?}");
    assert!(
        !document_path(root).exists(),
        "the failed server published nothing"
    );
    let recorded = fs::read_to_string(stderr_file_path(root))?;
    assert!(
        recorded.contains("failed to start"),
        "the stderr file carries the server's refusal: {recorded:?}"
    );
    failure_window.passed();
    Ok(())
}

#[test]
fn background_start_keeps_listening_and_reads_report_source_file_limit() -> TestResult {
    const SOURCE_FILES_MAX: usize = 1_000;
    let directory =
        workspace_with_server_keys(&format!("\n[source]\nfiles = {SOURCE_FILES_MAX}\n"))?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);
    for index in 0..SOURCE_FILES_MAX {
        fs::write(root.join(format!("unit_{index:04}.rs")), "")?;
    }

    let started = rift(root, &["server", "start"])?;
    require_success(&started, "start before background file discovery")?;
    let lock = serving_document(root).ok_or("the listener publishes before file discovery")?;

    let deadline = std::time::Instant::now() + START_WAIT_MAX;
    let mut refusal = None;
    while std::time::Instant::now() < deadline {
        let answer = symbol_request(lock.port, &lock.token, "beacon")?;
        let body = answer
            .split_once("\r\n\r\n")
            .ok_or("the MCP response must contain an HTTP body")?
            .1;
        let response: serde_json::Value = serde_json::from_str(body)?;
        let result = &response["result"];
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        if result["isError"] == true && text.starts_with("1 error\n\tlimit_exceeded · retry ") {
            refusal = Some((result.clone(), text.to_owned()));
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    let (result, text) = refusal.ok_or("background discovery must report the files limit")?;
    assert!(result.get("structuredContent").is_none(), "{result}");
    assert_eq!(
        result["content"].as_array().map(Vec::len),
        Some(1),
        "{result}"
    );
    assert!(!text.contains("phase"), "{text}");
    assert!(
        text.lines().any(|line| line
            == format!(
                "\t\tlimit source.files: {} over {SOURCE_FILES_MAX}",
                SOURCE_FILES_MAX + 1
            )),
        "{text}"
    );
    failure_window.passed();
    Ok(())
}

/// The zero-byte file under `.rift` a server claims the election on, by
/// locking it exclusively.
const ELECTION_FILE_NAME: &str = "server.lock";

/// Polls while waiting for a spawned server's stderr refusal.
const RECORD_READ_ATTEMPT_COUNT: u32 = 50;

/// A claim that meets any lock on the election file loses the start election,
/// a shared one included. This test keeps a shared lock on the file the way a
/// probe's lock outlives the probe on Windows, which releases a closed handle's
/// locks lazily, so the first server `rift server start` spawns loses an
/// election no process holds and exits. Once the lock goes, the start spawns
/// again inside its window and reports the server it elected.
// Leaked handles: https://github.com/volarized/rift/issues/484
#[test]
fn a_start_lost_to_a_lingering_shared_lock_spawns_again() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);
    fs::create_dir_all(root.join(".rift"))?;
    let lingering = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".rift").join(ELECTION_FILE_NAME))?;
    lingering.try_lock_shared()?;

    // The start writes into files: on Windows the detached server inherits the
    // starting process's handles, so a pipe would stay open until it leaves.
    let output = tempfile::tempdir()?;
    let stdout_path = output.path().join("start.stdout");
    let stderr_path = output.path().join("start.stderr");
    let mut start = Command::new(rift_binary()?)
        .args(["server", "start"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(fs::File::create(&stdout_path)?)
        .stderr(fs::File::create(&stderr_path)?)
        .spawn()?;
    let lost = wait_for(
        RECORD_READ_ATTEMPT_COUNT,
        "the lost election's stderr refusal",
        || {
            fs::read_to_string(stderr_file_path(root))
                .ok()
                .filter(|printed| printed.contains("server_already_serving"))
        },
    );
    assert!(
        !root.join(".rift/index").exists(),
        "a refused child cannot open the held workspace database"
    );
    lingering.unlock()?;
    drop(lingering);
    let status = start.wait()?;
    lost?;

    let stdout = fs::read_to_string(&stdout_path)?;
    let stderr = fs::read_to_string(&stderr_path)?;
    assert!(
        status.success(),
        "the start must serve once the lock goes: status {status:?}, stdout {stdout:?}, \
         stderr {stderr:?}"
    );
    let (_, pid) = listening_facts(&stdout)?;
    let serving = serving_document(root).ok_or("the started server must be live")?;
    assert_eq!(serving.pid, pid);
    failure_window.passed();
    Ok(())
}

#[test]
fn status_reports_absent_stale_and_serving_states() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

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
        server: None,
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
    let printed = rift(root, &["--version"])?;
    require_success(&printed, "version")?;
    assert_eq!(
        stdout_of(&printed).trim(),
        format!("rift {}", document.identity.version),
        "rift --version prints the version the server publishes"
    );

    let stopped = rift(root, &["server", "stop"])?;
    require_success(&stopped, "stop after the serving status")?;
    failure_window.passed();
    Ok(())
}

#[test]
fn a_variable_overrides_rift_toml_in_the_started_server() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    fs::write(root.join("deep.rs"), deep_source())?;
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let answer = started_answer_holding(
        root,
        &[("RIFT_PROVIDERS_SYNTAX_MAX_DEPTH", "2048")],
        "deep",
        "deep.rs",
    )?;
    assert!(answer.contains("deep.rs"), "{answer}");

    require_success(&rift(root, &["server", "stop"])?, "stop")?;
    failure_window.passed();
    Ok(())
}

#[test]
fn a_misspelled_variable_refuses_every_request_naming_it() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let answer = started_answer_holding(
        root,
        &[("RIFT_PROVIDERS_SYNTAX_MAX_NODE", "5000000")],
        "beacon",
        "configuration_invalid",
    )?;
    assert!(
        answer.contains("RIFT_PROVIDERS_SYNTAX_MAX_NODE"),
        "{answer}"
    );
    assert!(
        answer.contains("RIFT_PROVIDERS_SYNTAX_MAX_NODES"),
        "{answer}"
    );

    require_success(&rift(root, &["server", "stop"])?, "stop")?;
    failure_window.passed();
    Ok(())
}
