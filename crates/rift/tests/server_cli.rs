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
#[allow(
    dead_code,
    reason = "shared test identity helper, used by sibling suites"
)]
mod test_case;

use std::error::Error;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// A command running [`rift_binary`] that carries the test's
/// [`test_case::TEST_CASE_NAME_ATTRIBUTE`].
fn rift_command() -> TestResult<Command> {
    let mut command = Command::new(rift_binary()?);
    test_case::with_test_case_name(&mut command);
    Ok(command)
}

/// What a foreground server's failure window registers it as.
const FOREGROUND_LABEL: &str = "rift server start --foreground";

/// The exit status of `child` once it exited, recorded in the test's failure window.
fn exited(child: &mut Child) -> Option<std::process::ExitStatus> {
    let status = child.try_wait().ok().flatten()?;
    harness::record_exit(child.id(), status);
    Some(status)
}

/// The text of `server.json` in `root`, or why it could not be read, for a failure that
/// finds the document still there.
fn document_text(root: &Path) -> String {
    fs::read_to_string(document_path(root)).unwrap_or_else(|error| format!("not read: {error}"))
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
        let Ok(mut command) = rift_command() else {
            return;
        };
        let _ = command
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
    marks: Arc<Mutex<StderrMarks>>,
    reader: std::thread::JoinHandle<()>,
}

/// The message the server's stop opens with, the instant its stage deadline starts.
const STOPPING_MESSAGE: &[u8] = b"MCP server stopping";

/// When the reader saw the server reach points of its stop, on the test's monotonic clock.
#[derive(Clone, Copy, Debug, Default)]
struct StderrMarks {
    /// The first read that carried [`STOPPING_MESSAGE`].
    stopping: Option<std::time::Instant>,
    /// The last read that carried bytes.
    last_write: Option<std::time::Instant>,
    /// The read that found the stream closed: the server and every process holding its
    /// stderr had exited.
    closed: Option<std::time::Instant>,
}

impl StderrWatch {
    /// Drains the piped stderr of `child` on another thread so the foreground server can
    /// keep writing startup records. The retained prefix stays bounded for the failure
    /// message. The child is registered in the test's failure window under `label`, and
    /// every byte read lands in its stderr copy there.
    fn spawn(child: &mut Child, label: &str) -> TestResult<Self> {
        let mut stream: ChildStderr = child.stderr.take().ok_or("the child's stderr is piped")?;
        harness::register_process(child.id(), label);
        let mut copy = harness::stderr_copy(child.id());
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&bytes);
        let marks = Arc::new(Mutex::new(StderrMarks::default()));
        let marked = Arc::clone(&marks);
        let reader = std::thread::spawn(move || {
            let mut buffer = [0_u8; 8 << 10];
            // The bytes before this read that the stopping message could start in.
            let mut carried: Vec<u8> = Vec::new();
            loop {
                let count = match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                let now = std::time::Instant::now();
                if let Some(copy) = &mut copy {
                    copy.write(&buffer[..count]);
                }
                carried.extend_from_slice(&buffer[..count]);
                {
                    let mut marks = marked
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    marks.last_write = Some(now);
                    if marks.stopping.is_none()
                        && carried
                            .windows(STOPPING_MESSAGE.len())
                            .any(|window| window == STOPPING_MESSAGE)
                    {
                        marks.stopping = Some(now);
                    }
                }
                let keep = carried.len().min(STOPPING_MESSAGE.len());
                carried.drain(..carried.len() - keep);
                let mut retained = captured
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let room = STARTUP_STDERR_BYTES_MAX.saturating_sub(retained.len());
                retained.extend_from_slice(&buffer[..count.min(room)]);
            }
            marked
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closed = Some(std::time::Instant::now());
        });
        Ok(Self {
            bytes,
            marks,
            reader,
        })
    }

    /// When the reader saw the server reach each point of its stop so far.
    fn marks(&self) -> StderrMarks {
        *self
            .marks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        let Self { bytes, reader, .. } = self;
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
            let status = exited(child);
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
    Ok(rift_command()?
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

/// The server's own stop line, as its stderr rendered it: the record lies outside every
/// span, so its `key=value` fields, string values without quotes, print before the
/// message.
fn stop_line_of(stderr: &str) -> TestResult<&str> {
    stderr
        .lines()
        .find(|line| line.ends_with("MCP server stopped"))
        .ok_or_else(|| format!("serving must end before the process leaves: {stderr:?}").into())
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
    start_foreground_server_with(root, &[])
}

/// [`start_foreground_server`] with `variables` laid over [`SERVER_LOG_VARIABLES`].
fn start_foreground_server_with(
    root: &Path,
    variables: &[(&str, &str)],
) -> TestResult<(Child, ServerLock, StderrWatch)> {
    let mut child = rift_command()?
        .args(["server", "start", "--foreground"])
        .envs(SERVER_LOG_VARIABLES)
        .envs(variables.iter().copied())
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(&mut child, FOREGROUND_LABEL)?;
    match wait_for_foreground_server(root, &mut child, &stderr) {
        Ok(serving) => Ok((child, serving, stderr)),
        Err(error) => {
            let _ = child.kill();
            if let Ok(status) = child.wait() {
                harness::record_exit(child.id(), status);
            }
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
    let mut stop = rift_command()?
        .args(["server", "stop"])
        .envs(SERVER_LOG_VARIABLES)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut exits = StopExits::default();
    loop {
        let stop_status = stop.try_wait()?;
        let server_status = exited(child);
        exits.observe(stop_status.is_some(), server_status.is_some());
        if let (Some(stop_status), Some(server_status)) = (stop_status, server_status) {
            let stop_output = stop.wait_with_output()?;
            require_success(&stop_output, "stop the foreground server")?;
            let elapsed = exits.render(started, &stderr.marks());
            assert!(
                stop_status.success(),
                "stop command must exit cleanly: {stop_status:?}; {elapsed}"
            );
            assert!(
                server_status.success(),
                "foreground server must exit cleanly: {server_status:?} (the server's own exit \
                 status); {elapsed}; stderr: {}",
                stderr.after_exit()
            );
            assert!(
                started.elapsed() <= DATABASE_REOPEN_STOP_BOUND,
                "stop and observed process exit must fit {DATABASE_REOPEN_STOP_BOUND:?}; \
                 {elapsed}"
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let still_running = match (stop_status.is_some(), server_status) {
                (false, None) => "the stop command and the server",
                (false, Some(_)) => "the stop command",
                (true, None) => "the server",
                (true, Some(_)) => "neither",
            };
            let _ = stop.kill();
            let _ = stop.wait();
            // A status the server reported before the kill is its own; one after it is
            // the kill's, which Windows reports as exit status 1.
            let server_status = if let Some(status) = server_status {
                format!("{status:?} (the server's own exit status)")
            } else {
                let _ = child.kill();
                let status = child.wait()?;
                harness::record_exit(child.id(), status);
                format!("{status:?} (the harness killed the server)")
            };
            return Err(format!(
                "stop and process exit exceeded {DATABASE_REOPEN_STOP_BOUND:?}; still running \
                 at the bound: {still_running}; server status: {server_status}; {}; stderr: {}",
                exits.render(started, &stderr.marks()),
                stderr.after_exit()
            )
            .into());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// When the stop helper first observed each process exited, on the test's monotonic clock.
#[derive(Debug, Default)]
struct StopExits {
    stop: Option<std::time::Instant>,
    server: Option<std::time::Instant>,
}

impl StopExits {
    /// Records the first poll that found the stop command or the server exited.
    fn observe(&mut self, stop_exited: bool, server_exited: bool) {
        let now = std::time::Instant::now();
        if stop_exited {
            self.stop.get_or_insert(now);
        }
        if server_exited {
            self.server.get_or_insert(now);
        }
    }

    /// The milliseconds from the stop request at `started` to each point of the stop:
    /// `elapsed since the stop request: MCP server stopping 412 ms, last stderr write
    /// 4420 ms, stderr closed 4431 ms, server exit observed 4501 ms, stop command exit
    /// observed 4602 ms`; a point not reached reads `not reached`.
    fn render(&self, started: std::time::Instant, marks: &StderrMarks) -> String {
        let since = |instant: Option<std::time::Instant>| {
            instant.map_or_else(
                || "not reached".to_owned(),
                |instant| {
                    format!(
                        "{} ms",
                        instant.saturating_duration_since(started).as_millis()
                    )
                },
            )
        };
        format!(
            "elapsed since the stop request: MCP server stopping {}, last stderr write {}, \
             stderr closed {}, server exit observed {}, stop command exit observed {}",
            since(marks.stopping),
            since(marks.last_write),
            since(marks.closed),
            since(self.server),
            since(self.stop),
        )
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
        "a graceful stop retires server.json; server.json: {}",
        document_text(root)
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

    let mut child = rift_command()?
        .args(["server", "start", "--foreground"])
        .envs(SERVER_LOG_VARIABLES)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(&mut child, FOREGROUND_LABEL)?;
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

        let mut child = rift_command()?
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

/// Every OTLP/HTTP export request one receiver answered: when it arrived, its path, and its
/// body's length.
#[cfg(unix)]
type ReceivedExports =
    std::sync::Arc<std::sync::Mutex<Vec<(std::time::Instant, &'static str, usize)>>>;

/// An OTLP/HTTP receiver on a loopback port that records each span and metric export and
/// answers success.
#[cfg(unix)]
struct TraceReceiver {
    _runtime: tokio::runtime::Runtime,
    port: u16,
    exports: ReceivedExports,
}

#[cfg(unix)]
impl TraceReceiver {
    fn start() -> TestResult<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        let listener = runtime.block_on(tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))?;
        let port = listener.local_addr()?.port();
        let exports = ReceivedExports::default();
        let route = |path: &'static str| {
            let recorded = std::sync::Arc::clone(&exports);
            axum::routing::post(move |body: axum::body::Bytes| async move {
                recorded
                    .lock()
                    .expect("the recorded exports are not poisoned")
                    .push((std::time::Instant::now(), path, body.len()));
                axum::http::StatusCode::OK
            })
        };
        let receiver = axum::Router::new()
            .route("/v1/traces", route("/v1/traces"))
            .route("/v1/metrics", route("/v1/metrics"))
            .route("/v1/logs", route("/v1/logs"));
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

    /// The byte counts of the exports to `path` that arrived at or after `moment`.
    fn exports_since(&self, path: &str, moment: std::time::Instant) -> Vec<usize> {
        self.exports
            .lock()
            .expect("the recorded exports are not poisoned")
            .iter()
            .filter(|(arrived, received, _)| *arrived >= moment && *received == path)
            .map(|(_, _, bytes)| *bytes)
            .collect()
    }
}

/// The batch processor's and the metric reader's export interval the export tests set: ten
/// minutes, so no scheduled export runs while the server serves and only the stop's final
/// flush sends.
#[cfg(unix)]
const EXPORT_INTERVAL_PAST_THE_TEST_MS: &str = "600000";

/// The variables that point a server at `endpoint` with no scheduled export.
#[cfg(unix)]
fn export_variables(endpoint: &str) -> [(&str, &str); 3] {
    [
        ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint),
        ("OTEL_BSP_SCHEDULE_DELAY", EXPORT_INTERVAL_PAST_THE_TEST_MS),
        (
            "OTEL_METRIC_EXPORT_INTERVAL",
            EXPORT_INTERVAL_PAST_THE_TEST_MS,
        ),
    ]
}

#[cfg(unix)]
#[test]
fn sigterm_flushes_the_otlp_export_before_the_process_exits() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);
    let receiver = TraceReceiver::start()?;
    let endpoint = receiver.endpoint();

    let server = ListeningForeground::start(root, &export_variables(&endpoint))?;
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
    let spans = receiver.exports_since("/v1/traces", signalled);
    assert!(
        spans.iter().any(|bytes| *bytes > 0),
        "the export shutdown sends the spans the server closed while serving: {spans:?}"
    );
    let points = receiver.exports_since("/v1/metrics", signalled);
    assert_eq!(
        points.len(),
        1,
        "the export shutdown sends the final metric points once: {points:?}"
    );
    let records = stored_records(root)?;
    let export = stage_ended_line(&records, "otlp export")?;
    assert!(export.contains("outcome=ok"), "{export}");
    failure_window.passed();
    Ok(())
}

/// A collector that accepts the connection and never answers costs the stop the export's
/// reserve: the `otlp export` stage ends `timeout` at `warn`, and the process exits cleanly
/// inside the stop bound.
#[cfg(unix)]
#[test]
fn a_stalled_collector_ends_the_export_stage_timeout_inside_the_stop_bound() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let endpoint = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    // Every accepted connection stays open and unread until the test ends.
    std::thread::spawn(move || {
        let held: Vec<_> = listener.incoming().collect();
        drop(held);
    });

    let server = ListeningForeground::start(root, &export_variables(&endpoint))?;
    let (status, elapsed, stderr) = server.terminate()?;

    assert!(
        status.success(),
        "a stalled collector fails no stop: {status:?}, stderr: {stderr}"
    );
    assert!(
        elapsed <= STOP_EXIT_BOUND,
        "the export stage holds the stop for its reserve alone: elapsed={elapsed:?}, bound={STOP_EXIT_BOUND:?}"
    );
    let records = stored_records(root)?;
    let export = stage_ended_line(&records, "otlp export")?;
    assert!(export.contains("outcome=timeout"), "{export}");
    assert!(export.contains("WARN"), "{export}");
    failure_window.passed();
    Ok(())
}

/// A collector that refuses the connection costs the stop at most the export's reserve: the
/// exporter retries a refused export three times, 100 ms apart and doubling, so the stage
/// ends `error` when the retries end inside the reserve and `timeout` when they do not,
/// both at `warn`, and the process exits cleanly.
#[cfg(unix)]
#[test]
fn a_refused_collector_ends_the_export_stage_error_and_the_stop_cleanly() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);
    let refused = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let endpoint = format!("http://127.0.0.1:{}", refused.local_addr()?.port());
    drop(refused);

    let server = ListeningForeground::start(root, &export_variables(&endpoint))?;
    let (status, elapsed, stderr) = server.terminate()?;

    assert!(
        status.success(),
        "a refused collector fails no stop: {status:?}, stderr: {stderr}"
    );
    assert!(
        elapsed <= STOP_EXIT_BOUND,
        "elapsed={elapsed:?}, bound={STOP_EXIT_BOUND:?}"
    );
    let records = stored_records(root)?;
    let export = stage_ended_line(&records, "otlp export")?;
    assert!(
        export.contains("outcome=error") || export.contains("outcome=timeout"),
        "{export}"
    );
    assert!(export.contains("WARN"), "{export}");
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

    let mut child = rift_command()?
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .envs(SERVER_LOG_VARIABLES)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(&mut child, FOREGROUND_LABEL)?;
    let serving = wait_for_foreground_server(root, &mut child, &stderr)?;
    assert_eq!(serving.pid, child.id(), "the child itself must serve");

    // The server serves past its own stop span before anyone asks it to stop, so a
    // deadline derived at startup would leave every later stage nothing to spend.
    std::thread::sleep(IDLE_SPAN_PAST_STOP_DEADLINE);
    stop_foreground_server(root, &mut child, &stderr)?;

    wait_for(
        GONE_POLL_ATTEMPT_COUNT,
        "the foreground child to exit",
        || exited(&mut child),
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

/// A `rustup` on a fixture `PATH` directory, ahead of the inherited `PATH`, whose `body`
/// runs under `sh`; answers the `PATH` value that finds it first.
#[cfg(unix)]
fn fixture_rustup(tools: &Path, body: &str) -> TestResult<String> {
    use std::os::unix::fs::PermissionsExt as _;

    let program = tools.join("rustup");
    fs::write(&program, format!("#!/bin/sh\n{body}"))?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755))?;
    let inherited = std::env::var_os("PATH").ok_or("the test process has a PATH")?;
    let paths = std::iter::once(tools.to_path_buf()).chain(std::env::split_paths(&inherited));
    std::env::join_paths(paths)?
        .into_string()
        .map_err(|path| format!("the fixture PATH is not UTF-8: {}", path.display()).into())
}

/// The stored records `rift server logs` prints for the workspace, once the server left.
#[cfg(unix)]
fn stored_records(root: &Path) -> TestResult<String> {
    let printed = rift(root, &["server", "logs"])?;
    require_success(&printed, "read the stored records")?;
    Ok(stdout_of(&printed))
}

/// The stored `stop stage ended` line of `stage`.
#[cfg(unix)]
fn stage_ended_line<'a>(records: &'a str, stage: &str) -> TestResult<&'a str> {
    records
        .lines()
        .find(|line| line.contains("stop stage ended") && line.contains(stage))
        .ok_or_else(|| format!("the store holds no end of the {stage:?} stage: {records}").into())
}

/// A stop that lands while the initial preparation waits on a dependency version probe
/// kills the probe's child: the `index supervisor shutdown` stage ends `ok` with time
/// left, the probe's close records the cancellation, and the store holds the stop's own
/// last record. The fixture `rustup` `exec`s its sleep, so the probe's child is the
/// sleeping process itself.
#[cfg(unix)]
#[test]
fn a_stop_during_a_dependency_probe_kills_the_probe_and_ends_cleanly() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let tools = tempfile::tempdir()?;
    let started = tools.path().join("probe-started");
    let path = fixture_rustup(
        tools.path(),
        &format!("printf started > '{}'\nexec sleep 30\n", started.display()),
    )?;
    let (mut child, _serving, stderr) =
        start_foreground_server_with(root, &[("PATH", path.as_str())])?;
    wait_for(
        START_POLL_ATTEMPT_COUNT,
        "the rustup probe to start",
        || started.exists().then_some(()),
    )?;

    stop_foreground_server(root, &mut child, &stderr)?;

    let records = stored_records(root)?;
    let supervisor = stage_ended_line(&records, "index supervisor shutdown")?;
    assert!(supervisor.contains("outcome=ok"), "{supervisor}");
    assert!(
        !supervisor.contains("remaining=0ns"),
        "the stage ends with time left: {supervisor}"
    );
    let probe = records
        .lines()
        .find(|line| line.contains("dependency.probe") && line.contains("program=rustup"))
        .ok_or_else(|| format!("the store holds the probe's close: {records}"))?;
    assert!(probe.contains("close ✗ cancelled"), "{probe}");
    assert!(
        records
            .lines()
            .any(|line| line.ends_with("MCP server stopped")),
        "{records}"
    );
    Ok(())
}

/// A stop that lands while a probe's child left a process holding the probe's output
/// pipes still ends cleanly. The fixture `rustup` starts a background sleep that inherits
/// both pipes, then waits on it: the stop kills the probe's child, the probe stops waiting
/// on the held pipes after its bound, its close records the cancellation, and the
/// `index supervisor shutdown` stage ends `ok` with time left. On Unix the probe runner
/// does not reach the sleep, so the test ends it by the pid the fixture recorded.
#[cfg(unix)]
#[test]
fn a_stop_during_a_probe_whose_child_holds_the_pipes_ends_cleanly() -> TestResult {
    let directory = workspace()?;
    let root = directory.path();
    let _cleanup = StopOnDrop::new(root);
    let tools = tempfile::tempdir()?;
    let started = tools.path().join("probe-started");
    let holder = tools.path().join("holder-pid");
    let path = fixture_rustup(
        tools.path(),
        &format!(
            "sleep 30 &\nprintf %s \"$!\" > '{}'\nprintf started > '{}'\nwait\n",
            holder.display(),
            started.display()
        ),
    )?;
    let (mut child, _serving, stderr) =
        start_foreground_server_with(root, &[("PATH", path.as_str())])?;
    let held = wait_for(
        START_POLL_ATTEMPT_COUNT,
        "the rustup probe to start",
        || started.exists().then_some(()),
    )
    .and_then(|()| {
        let stop = rift(root, &["server", "stop"])?;
        let server = wait_for(
            GONE_POLL_ATTEMPT_COUNT,
            "the foreground server to exit",
            || exited(&mut child),
        )?;
        Ok((stop, server))
    });
    // The sleep the fixture left behind is the server's grandchild: end it by the pid it
    // recorded, whatever the stop did.
    if let Some(pid) = fs::read_to_string(&holder)
        .ok()
        .and_then(|pid| pid.trim().parse::<i32>().ok())
    {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let (stop, _server) = held.map_err(|error| {
        format!(
            "{error}; stderr: {}",
            harness::bounded_tail(&stderr.snapshot())
        )
    })?;
    require_success(&stop, "stop the foreground server")?;

    let records = stored_records(root)?;
    let supervisor = stage_ended_line(&records, "index supervisor shutdown")?;
    assert!(supervisor.contains("outcome=ok"), "{supervisor}");
    assert!(
        !supervisor.contains("remaining=0ns"),
        "the stage ends with time left: {supervisor}"
    );
    let probe = records
        .lines()
        .find(|line| line.contains("dependency.probe") && line.contains("program=rustup"))
        .ok_or_else(|| format!("the store holds the probe's close: {records}"))?;
    assert!(probe.contains("close ✗ cancelled"), "{probe}");
    assert!(
        records
            .lines()
            .any(|line| line.ends_with("MCP server stopped")),
        "{records}"
    );
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

    let mut child = rift_command()?
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .envs(SERVER_LOG_VARIABLES)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(&mut child, FOREGROUND_LABEL)?;
    let serving = wait_for_foreground_server(root, &mut child, &stderr)?;
    assert_eq!(serving.pid, child.id(), "the child itself must serve");

    // The document appears after HTTP binds, while source preparation can still run.
    // The lane's injected cancellation tests separately prove held transaction abort.
    stop_foreground_server(root, &mut child, &stderr)?;

    // The stop helper already observes CLI completion and process exit within five seconds.
    let status = wait_for(
        GONE_POLL_ATTEMPT_COUNT,
        "the stopped server's process to exit after the listener binds",
        || exited(&mut child),
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
        "a graceful stop retires server.json; server.json: {}",
        document_text(root)
    );
    failure_window.passed();
    Ok(())
}

/// How long the server's index writes wait for another process's lock in the forced
/// close below: `[search] busy_timeout` at its upper bound, longer than the whole stop.
const HELD_INDEX_BUSY_TIMEOUT: &str = "30s";

/// A server whose index write waits on a lock another process holds is stopped: the
/// write keeps its SQLite worker, so the worker's stop outlasts its bound; the close runs no
/// checkpoint. The stop still exits 0, ends the `SQLite worker shutdown` stage `timeout`
/// with the held worker named, and
/// retires `server.json` before the process leaves, while the held worker keeps the
/// election until the process exits.
#[test]
fn a_stop_whose_index_close_outlasts_its_bound_ends_timeout_and_retires_the_document() -> TestResult
{
    let directory = workspace()?;
    let root = directory.path();
    let configuration = fs::read_to_string(root.join("rift.toml"))?;
    fs::write(
        root.join("rift.toml"),
        format!("{configuration}[search]\nbusy_timeout = \"{HELD_INDEX_BUSY_TIMEOUT}\"\n"),
    )?;
    let _cleanup = StopOnDrop::new(root);
    let failure_window = harness::FailureWindow::begin(root);

    let mut child = rift_command()?
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .envs(SERVER_LOG_VARIABLES)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(&mut child, FOREGROUND_LABEL)?;
    wait_for_foreground_server(root, &mut child, &stderr)?;
    wait_for(START_POLL_ATTEMPT_COUNT, "the first lexical commit", || {
        stderr
            .snapshot()
            .contains("lexical commit settled")
            .then_some(())
    })?;

    // Another process takes the index database's write lock, then a source change makes
    // the server write: its `BEGIN IMMEDIATE` waits inside SQLite's busy handler.
    let holder = rusqlite::Connection::open(root.join(".rift").join("index"))?;
    holder.execute_batch("BEGIN IMMEDIATE")?;
    fs::write(
        root.join("lib.rs"),
        "pub fn beacon() {}\npub fn held() {}\n",
    )?;
    wait_for(START_POLL_ATTEMPT_COUNT, "the held lexical commit", || {
        (stderr
            .snapshot()
            .matches("lexical commit committing")
            .count()
            >= 2)
            .then_some(())
    })?;

    stop_foreground_server(root, &mut child, &stderr)?;
    let status = wait_for(
        GONE_POLL_ATTEMPT_COUNT,
        "the stopped server to exit",
        || exited(&mut child),
    )?;
    holder.execute_batch("ROLLBACK")?;
    let stderr = stderr.finished()?;
    assert!(
        status.success(),
        "the stop exits cleanly: {status:?}; {stderr}"
    );
    let worker_stage = stderr
        .lines()
        .find(|line| {
            line.contains("stage=SQLite worker shutdown") && line.contains("stop stage ended")
        })
        .ok_or_else(|| format!("the SQLite worker stage ended with a record: {stderr}"))?;
    assert!(worker_stage.contains("outcome=timeout"), "{worker_stage}");
    assert!(
        stderr.contains("SQLite worker outlasted the shutdown deadline")
            && stderr.contains("database closed; the write-ahead log stays for the next open"),
        "the held worker and the close without a checkpoint are named: {stderr}"
    );
    // The held writer still waits inside SQLite's busy handler when the process leaves:
    // a thread blocked in user space does not hold the exit, and the exit is the stop's
    // last record.
    let exit = stderr
        .lines()
        .rfind(|line| line.contains("process exits"))
        .ok_or_else(|| format!("the exit is recorded: {stderr}"))?;
    assert!(exit.contains("status=0"), "{exit}");
    assert!(
        stderr
            .lines()
            .any(|line| line.contains("stage=tracing shutdown") && line.contains("outcome=ok")),
        "the tracing shutdown ends inside its bound: {stderr}"
    );
    // The held worker keeps its clone of the election guard, so the election is
    // released by the process exit, not by the stop.
    assert!(
        stderr.contains("a database thread still holds the workspace election"),
        "the stop names the held election: {stderr}"
    );
    assert!(
        !document_path(root).exists(),
        "a stop that ends timeout retires server.json; server.json: {:?}; stderr: {stderr}",
        fs::read_to_string(document_path(root))
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

    let mut child = rift_command()?
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .envs(SERVER_LOG_VARIABLES)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(&mut child, FOREGROUND_LABEL)?;
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
        || exited(&mut child),
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
        "a graceful stop retires server.json; server.json: {}",
        document_text(root)
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

    let mut child = rift_command()?
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .env("RUST_LOG", STARTUP_TRACE_FILTER)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(&mut child, FOREGROUND_LABEL)?;
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
            let exit = exited(&mut child);
            observations.push((document_present, exit.is_some()));
            exit
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
        "a graceful stop retires server.json; server.json: {}",
        document_text(root)
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

    let mut child = rift_command()?
        .args(["server", "start", "--foreground"])
        .current_dir(root)
        .env("RUST_LOG", STARTUP_TRACE_FILTER)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let stderr = StderrWatch::spawn(&mut child, FOREGROUND_LABEL)?;
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
        || exited(&mut child),
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
        "a graceful stop retires server.json; server.json: {}",
        document_text(root)
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
            rift_command()?
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
/// An attempt id in the form nextest documents: run id, binary id, stress index, test name.
const FIXTURE_ATTEMPT: &str =
    "55459fda-13fe-406a-b4e3-0230fd52bb03:rift::server_cli@stress-3$a_case";

/// The window files of [`FIXTURE_ATTEMPT`] in the `default` profile below `reports`.
fn fixture_window_files(reports: &Path) -> harness::WindowFiles {
    harness::WindowFiles::new(
        reports.join("default").join("failure-windows"),
        FIXTURE_ATTEMPT,
    )
}

/// A window that fails in its test's own process keeps its start and appends `ended_at`,
/// writes what it printed beside it, and prints each registered process's label, the exit
/// the test observed or that it observed none, and the tail of its stderr copy.
#[test]
fn a_failing_window_keeps_its_files_and_prints_each_registered_process() -> TestResult {
    let reports = tempfile::tempdir()?;
    let files = fixture_window_files(reports.path());
    let window = harness::FailureWindow::begin_in(Some(files.clone()), Some(FIXTURE_ATTEMPT), &[]);
    files.register_process(4242, FOREGROUND_LABEL)?;
    files.register_process(4343, "rift mcp")?;
    let relay = harness::RelayedStderr::spawn(
        &b"MCP server stopping\nMCP server stopped\n"[..],
        files.stderr_copy(4242)?,
    );
    let relayed = tokio::runtime::Builder::new_current_thread()
        .build()?
        .block_on(relay.text())?;
    assert_eq!(relayed, "MCP server stopping\nMCP server stopped\n");
    let status = std::process::ExitStatus::default();
    files.record_exit(4242, status)?;
    files.record_exit(4242, status)?;
    files.record_exit(5555, status)?;

    drop(window);

    let start = harness::WindowStart::read(&files.start())?;
    assert!(start.ended_at.is_some(), "{start:?}");
    assert_eq!(start.attempt.as_deref(), Some(FIXTURE_ATTEMPT));
    assert_eq!(
        start.processes,
        [
            harness::RegisteredProcess {
                pid: 4242,
                label: FOREGROUND_LABEL.to_owned(),
                exit: Some(format!("{status:?}")),
            },
            harness::RegisteredProcess {
                pid: 4343,
                label: "rift mcp".to_owned(),
                exit: None,
            },
        ]
    );
    let text = fs::read_to_string(files.text())?;
    for expected in [
        format!("---- process 4242: {FOREGROUND_LABEL} ----\nexit: {status:?}\n"),
        format!(
            "---- {} ----\nMCP server stopping\nMCP server stopped\n",
            files.stderr(4242).display()
        ),
        "---- process 4343: rift mcp ----\nexit: still running at the window".to_owned(),
        format!(
            "---- {} ----\nabsent: the harness drained no stderr",
            files.stderr(4343).display()
        ),
    ] {
        assert!(text.contains(&expected), "{expected:?} in {text}");
    }
    assert!(
        harness::killed_window_starts(reports.path())?.is_empty(),
        "a window carrying ended_at is left to the reader of its files"
    );
    let ended_at = start.ended_at.ok_or("the start carries ended_at")?;
    for expected in [
        format!("\nT: {ended_at}\n"),
        format!(
            "\nmachine: logical_cpus={} memory_bytes=not read by the harness system={} \
             architecture={}\n",
            std::thread::available_parallelism()?,
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    ] {
        assert!(text.contains(&expected), "{expected:?} in {text}");
    }
    if let Ok(name) = std::env::var("NEXTEST_TEST_NAME") {
        assert!(
            text.contains(&format!("NEXTEST_TEST_NAME={name}")),
            "the heading names the test: {text}"
        );
    }
    Ok(())
}

/// A stderr copy past its bound keeps the bytes before it, says where it cut, and ends
/// with the count of bytes it read and dropped; the window prints that count.
#[test]
fn a_cut_stderr_copy_counts_the_bytes_it_dropped() -> TestResult {
    let reports = tempfile::tempdir()?;
    let files = fixture_window_files(reports.path());
    let window = harness::FailureWindow::begin_in(Some(files.clone()), Some(FIXTURE_ATTEMPT), &[]);
    files.register_process(4242, FOREGROUND_LABEL)?;
    let mut copy = files
        .stderr_copy(4242)?
        .ok_or("a window with a start keeps a stderr copy")?;
    let bound = usize::try_from(harness::STDERR_FILE_BYTES_MAX)?;
    copy.write(&vec![b'a'; bound - 1]);
    copy.write(b"bcd");
    copy.write(b"efgh");
    drop(copy);

    drop(window);

    let kept = fs::read(files.stderr(4242))?;
    assert_eq!(kept[bound - 1], b'b', "the bytes before the bound are kept");
    let text = fs::read_to_string(files.text())?;
    for expected in [
        format!("[the copy reached its {bound}-byte bound; the rest is read and dropped]\n"),
        format!("[6 bytes past the {bound}-byte bound were read and dropped]\n"),
    ] {
        assert!(text.contains(&expected), "{expected:?} in the window");
    }
    Ok(())
}

/// A copy within its bound states no cut and no dropped bytes.
#[test]
fn a_stderr_copy_within_its_bound_states_no_drop() -> TestResult {
    let reports = tempfile::tempdir()?;
    let files = fixture_window_files(reports.path());
    let window = harness::FailureWindow::begin_in(Some(files.clone()), Some(FIXTURE_ATTEMPT), &[]);
    files.register_process(4242, FOREGROUND_LABEL)?;
    let mut copy = files
        .stderr_copy(4242)?
        .ok_or("a window with a start keeps a stderr copy")?;
    copy.write(b"MCP server ready\n");
    drop(copy);

    drop(window);

    let text = fs::read_to_string(files.text())?;
    assert!(text.contains("MCP server ready\n"), "{text}");
    assert!(!text.contains("read and dropped"), "{text}");
    Ok(())
}

/// Timestamped lines of the sources interleave by timestamp; a line without one stays
/// after the line before it; lines ahead of a source's first timestamp come first; one
/// timestamp keeps the order of the sources; blank lines are left out.
#[test]
fn merged_lines_interleave_by_timestamp() {
    let records = "[the first 9 bytes are cut by the 65536-byte bound]\n\
                   2026-10-05 10:00:00.100Z INFO a\n\n\
                   2026-10-05 10:00:00.300Z INFO c\n";
    let proxy = "2026-10-05 10:00:00.200Z INFO b\n  continued\n\
                 2026-10-05 10:00:00.300Z INFO d\n";
    assert_eq!(
        harness::merged_by_timestamp(&[("", records), ("rift mcp 7 | ", proxy)]),
        "[the first 9 bytes are cut by the 65536-byte bound]\n\
         2026-10-05 10:00:00.100Z INFO a\n\
         rift mcp 7 | 2026-10-05 10:00:00.200Z INFO b\n\
         rift mcp 7 |   continued\n\
         2026-10-05 10:00:00.300Z INFO c\n\
         rift mcp 7 | 2026-10-05 10:00:00.300Z INFO d\n"
    );
    assert_eq!(harness::merged_by_timestamp(&[("", ""), ("p ", "\n")]), "");
    assert_eq!(harness::printed_timestamp("2026-10-05 10:00:00.1Z x"), None);
    assert_eq!(harness::printed_timestamp("short"), None);
}

/// A window covering one workspace merges each registered `rift mcp` child's stderr into
/// that workspace's records by timestamp and says so under the process; a stderr file
/// holding the store's refusal is printed after the line saying the window falls back to
/// it.
#[test]
fn a_one_workspace_window_merges_proxy_stderr_and_falls_back_on_a_store_refusal() -> TestResult {
    let reports = tempfile::tempdir()?;
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let files = fixture_window_files(reports.path());
    let window =
        harness::FailureWindow::begin_in(Some(files.clone()), Some(FIXTURE_ATTEMPT), &[root]);
    files.register_process(4343, "rift mcp")?;
    files
        .stderr_copy(4343)?
        .ok_or("a window with a start keeps a stderr copy")?
        .write(b"2026-10-05 10:00:00.200Z INFO proxy line\n");
    let refusal = "rift: the log store refused a batch of 3; the log drain keeps it and \
                   retries every 250 ms: database is locked\n";
    let stderr_file = rift_mcp::stderr_file_path(root);
    fs::create_dir_all(
        stderr_file
            .parent()
            .ok_or("the stderr file has a directory")?,
    )?;
    fs::write(&stderr_file, refusal)?;

    drop(window);

    let text = fs::read_to_string(files.text())?;
    for expected in [
        "[merged by timestamp with the stderr of each line opening with `rift mcp 4343 | `]\n"
            .to_owned(),
        "rift mcp 4343 | 2026-10-05 10:00:00.200Z INFO proxy line\n".to_owned(),
        "---- process 4343: rift mcp ----".to_owned(),
        "merged by timestamp into the workspace's records above, each line opening with \
         `rift mcp 4343 | `\n"
            .to_owned(),
        format!(
            "---- {} ----\n[the log store refused writes (`rift: the log store refused a \
             batch`): the window falls back to this stderr file for the records the store \
             lacks]\n{refusal}",
            stderr_file.display()
        ),
    ] {
        assert!(text.contains(&expected), "{expected:?} in {text}");
    }
    Ok(())
}

/// A window that closes passed removes its start, its text, and every stderr copy.
#[test]
fn a_passing_window_removes_every_file_it_wrote() -> TestResult {
    let reports = tempfile::tempdir()?;
    let files = fixture_window_files(reports.path());
    let window = harness::FailureWindow::begin_in(Some(files.clone()), Some(FIXTURE_ATTEMPT), &[]);
    files.register_process(4242, FOREGROUND_LABEL)?;
    files
        .stderr_copy(4242)?
        .ok_or("a window with a start keeps a stderr copy")?
        .write(b"MCP server ready\n");
    files.record_exit(4242, std::process::ExitStatus::default())?;
    assert!(files.stderr(4242).exists());

    window.passed();

    let left: Vec<PathBuf> = fs::read_dir(reports.path().join("default").join("failure-windows"))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<_, _>>()?;
    assert!(left.is_empty(), "{left:?}");
    Ok(())
}

/// A start file reads back every key; a start without `ended_at` is a killed test's,
/// and a line the contract does not name is refused.
#[test]
fn a_start_file_reads_back_every_key() -> TestResult {
    let reports = tempfile::tempdir()?;
    let files = fixture_window_files(reports.path());
    fs::create_dir_all(files.start().parent().ok_or("the start has a directory")?)?;
    let start = format!(
        "test=a_case NEXTEST_ATTEMPT_ID={FIXTURE_ATTEMPT}\nattempt={FIXTURE_ATTEMPT}\n\
         started_at=2026-10-05T10:00:00Z\nroot=/tmp/fixture\n\
         process=4242 {FOREGROUND_LABEL}\nexit=4242 ExitStatus(unix_wait_status(0))\n"
    );
    fs::write(files.start(), &start)?;

    let read = harness::WindowStart::read(&files.start())?;
    assert_eq!(
        read.test,
        format!("a_case NEXTEST_ATTEMPT_ID={FIXTURE_ATTEMPT}")
    );
    assert_eq!(read.attempt.as_deref(), Some(FIXTURE_ATTEMPT));
    assert_eq!(
        read.started_at,
        "2026-10-05T10:00:00Z".parse::<jiff::Timestamp>()?
    );
    assert_eq!(read.roots, [PathBuf::from("/tmp/fixture")]);
    assert_eq!(
        read.processes,
        [harness::RegisteredProcess {
            pid: 4242,
            label: FOREGROUND_LABEL.to_owned(),
            exit: Some("ExitStatus(unix_wait_status(0))".to_owned()),
        }]
    );
    assert_eq!(read.ended_at, None);
    assert_eq!(
        harness::killed_window_starts(reports.path())?,
        [files.start()]
    );

    fs::write(
        files.start(),
        format!("{start}ended_at=2026-10-05T10:00:01Z\n"),
    )?;
    let ended = harness::WindowStart::read(&files.start())?;
    assert_eq!(ended.ended_at, Some("2026-10-05T10:00:01Z".parse()?));
    assert!(harness::killed_window_starts(reports.path())?.is_empty());

    for refused in [
        "exit=5555 ExitStatus(unix_wait_status(0))\n",
        "stage=stopping\n",
    ] {
        fs::write(files.start(), format!("{start}{refused}"))?;
        assert!(
            harness::WindowStart::read(&files.start()).is_err(),
            "{refused:?} is refused"
        );
    }
    Ok(())
}

/// The variable carries the test case name verbatim when it holds no character the SDK's
/// parse splits or trims at, writes those as `%XX`, and keeps inherited entries ahead.
#[test]
fn the_test_case_name_attribute_encodes_what_the_sdk_parse_splits_at() {
    assert_eq!(
        test_case::resource_attributes(None, FIXTURE_ATTEMPT),
        format!("test.case.name={FIXTURE_ATTEMPT}")
    );
    assert_eq!(
        test_case::resource_attributes(Some("service.namespace=ci"), "a, b%c\n"),
        "service.namespace=ci,test.case.name=a%2C%20b%25c%0A"
    );
    assert_eq!(
        test_case::resource_attributes(Some(" "), "a"),
        "test.case.name=a"
    );
}

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

fn debug_lingering_lock(stage: &str, path: &Path) {
    if !debug_lingering_lock_test() {
        return;
    }
    let unix_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    eprintln!(
        "DEBUG lingering shared lock {stage} unix_ns={unix_ns} pid={} path={}",
        std::process::id(),
        path.display(),
    );
}

fn debug_lingering_lock_test() -> bool {
    let test = "$a_start_lost_to_a_lingering_shared_lock_spawns_again";
    std::env::var_os("NEXTEST_ATTEMPT_ID")
        .is_some_and(|attempt| attempt.to_string_lossy().ends_with(test))
}

fn debug_lingering_output(label: &str, path: &Path) {
    if !debug_lingering_lock_test() {
        return;
    }
    let unix_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let content = fs::read_to_string(path).unwrap_or_else(|error| format!("<read error: {error}>"));
    eprintln!(
        "DEBUG lingering start output {label} unix_ns={unix_ns} pid={} path={} content={content:?}",
        std::process::id(),
        path.display(),
    );
}

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
    let election_path = root.join(".rift").join(ELECTION_FILE_NAME);
    debug_lingering_lock("acquired", &election_path);

    // The start writes into files: on Windows the detached server inherits the
    // starting process's handles, so a pipe would stay open until it leaves.
    let output = tempfile::tempdir()?;
    let stdout_path = output.path().join("start.stdout");
    let stderr_path = output.path().join("start.stderr");
    let mut start = rift_command()?
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
    for name in ["index", "metrics", "vectors"] {
        assert!(
            !root.join(".rift").join(name).exists(),
            "a refused child opens no database: {name}"
        );
    }
    debug_lingering_lock("unlock requested", &election_path);
    let unlocked = lingering.unlock();
    debug_lingering_lock(
        if unlocked.is_ok() {
            "unlock returned ok"
        } else {
            "unlock returned error"
        },
        &election_path,
    );
    unlocked?;
    debug_lingering_lock("handle drop requested", &election_path);
    drop(lingering);
    debug_lingering_lock("handle dropped", &election_path);
    let status = start.wait()?;
    debug_lingering_output("stdout", &stdout_path);
    debug_lingering_output("stderr", &stderr_path);
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
        concat!("rift ", env!("CARGO_PKG_VERSION")),
        "rift --version prints the package version alone"
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
