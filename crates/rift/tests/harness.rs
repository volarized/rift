//! The shared end-to-end harness: workspace fixtures, the real `rift mcp`
//! child, and the proxied call helpers every end-to-end suite drives
//! through.
//!
//! `mcp_proxy.rs` and `end_to_end.rs` each declare `mod harness;` and reach
//! this module's items through `crate::` from their own test bodies -
//! Cargo compiles each crate's integration test binary separately, so this
//! file is compiled once per binary that declares it, the same way
//! `engine_fixture.rs`, `live_engine_gate.rs`, and `rust_engine.rs` already
//! are. Adding a capability an end-to-end case needs means adding it here,
//! in this module's own vocabulary, not building a second harness.

use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::io::{Read, Seek as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rift_protocol::error::{ErrorCode, RetryDirective};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ReadResourceRequestParams, ReadResourceResult,
    ResourceContents,
};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::{ServiceExt as _, transport::child_process::TokioChildProcessBuilder};
use serde_json::json;

pub(crate) type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The executable supplied by the test runner, remapped when using an archive.
pub(crate) fn rift_binary() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_rift")
        .expect("test runner must provide CARGO_BIN_EXE_rift")
        .into()
}

/// Bound on one proxied round trip that may include a server election.
///
/// The bounds nest, each inside the next, so the innermost one that trips is
/// the one a failing case reports: the proxy's forward budget, which
/// [`FIXTURE_READINESS_TIMEOUT`] and [`FIXTURE_WORKER_QUEUE_TIMEOUT`] set,
/// ends inside this bound, and this bound ends inside nextest's one-minute
/// deadline. It also covers the proxy's start window, the longest a refusal to
/// start takes. `mcp_proxy`'s `proxied_forward_budget_ends_inside_the_call_bound`
/// pins the first relation and `dev/tests/test_delivery.py` the second.
pub(crate) const PROXIED_CALL_MAX: Duration = Duration::from_secs(45);
/// Bound on one proxied call that starts and settles a language engine.
pub(crate) const PROXIED_ENGINE_CALL_MAX: Duration = Duration::from_mins(2);

/// The declaration every non-engine fixture serves, and the file
/// referencing it.
pub(crate) const LIBRARY: &str = "pub fn beacon() {}\n";

/// `[server] readiness_timeout` of every fixture but the engine one.
///
/// With [`FIXTURE_WORKER_QUEUE_TIMEOUT`] and the proxy's answer grace it makes
/// the forward budget the proxy gives each request, which ends inside
/// [`PROXIED_CALL_MAX`]. The shipped defaults make a budget past both that
/// bound and nextest's deadline, so a server that stopped answering would end
/// a case with no report. The fixture indexes a handful of files, far inside
/// this bound.
pub(crate) const FIXTURE_READINESS_TIMEOUT: rift_protocol::configuration::Duration =
    rift_protocol::configuration::Duration::from_millis(15_000);
/// `[server] worker_queue_timeout` of every fixture but the engine one; see
/// [`FIXTURE_READINESS_TIMEOUT`].
pub(crate) const FIXTURE_WORKER_QUEUE_TIMEOUT: rift_protocol::configuration::Duration =
    rift_protocol::configuration::Duration::from_millis(5_000);

/// A workspace fixture: one Rust source and a `rift.toml` whose
/// `[server]` idle timeout reaps any orphaned server within a minute and
/// whose server binds an [`assigned_port`].
pub(crate) fn workspace() -> TestResult<tempfile::TempDir> {
    laid_out_workspace(&[("lib.rs", LIBRARY)], &assigned_port_key()?)
}

/// A loopback port the operating system assigned a moment ago and released.
///
/// Nextest runs each test in its own process, in parallel, and a server on the
/// default range binds its first free port, so servers from concurrent suites
/// hand ports between them; a fixture pins a port of its own instead.
pub(crate) fn assigned_port() -> TestResult<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// The `[server]` key pinning an [`assigned_port`], as `laid_out_workspace`
/// takes it.
pub(crate) fn assigned_port_key() -> TestResult<String> {
    Ok(format!("port = {}\n", assigned_port()?))
}

/// The cargo project the real rust-analyzer end-to-end cases serve:
/// rust-analyzer resolves nothing outside a cargo project, so the fixture
/// is one, with the same shape `rift-mcp`'s own `live_rust_analyzer.rs`
/// uses - a manifest whose `[lib]` path keeps every module file at the
/// tree root, and whose empty `[workspace]` table stops cargo climbing
/// out of the tempdir. `hub.rs` holds the declaration and `caller.rs`
/// imports and calls it, so an incoming reference reaches the caller.
pub(crate) const RUST_PROJECT_MANIFEST: &str = "[package]\nname = \"rift_live_fixture\"\nversion = \"0.0.0\"\n\
     edition = \"2021\"\npublish = false\n\n[lib]\npath = \"lib.rs\"\n\n\
     [workspace]\n";
pub(crate) const RUST_PROJECT_ROOT: &str = "pub mod caller;\npub mod hub;\n";
pub(crate) const RUST_PROJECT_HUB: &str = "pub fn beacon(value: i32) -> i32 {\n    value\n}\n";
pub(crate) const RUST_PROJECT_CALLER: &str =
    "use crate::hub::beacon;\n\npub fn total() -> i32 {\n    beacon(2)\n}\n";
pub(crate) fn rust_project() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Cargo.toml", RUST_PROJECT_MANIFEST),
        ("lib.rs", RUST_PROJECT_ROOT),
        ("hub.rs", RUST_PROJECT_HUB),
        ("caller.rs", RUST_PROJECT_CALLER),
    ]
}

/// Cargo project fixture with real Rust language LSP configuration appended,
/// serving `rust` through rust-analyzer.
///
/// It keeps the shipped `[server]` request bounds: a request here waits for
/// rust-analyzer to be ready, which [`PROXIED_ENGINE_CALL_MAX`] covers and
/// [`FIXTURE_READINESS_TIMEOUT`] does not.
pub(crate) fn rust_engine_workspace() -> TestResult<tempfile::TempDir> {
    fixture_workspace(
        &rust_project(),
        "",
        &format!(
            "{}{}",
            assigned_port_key()?,
            crate::rust_engine::rust_engine_configuration()
        ),
    )
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

/// One fixture workspace holding `files` and a `rift.toml` carrying the disabled
/// vector ranking, the orphan-safety idle timeout, the request bounds
/// [`FIXTURE_READINESS_TIMEOUT`] and [`FIXTURE_WORKER_QUEUE_TIMEOUT`], and
/// `extra_toml` - an LSP configuration, a `[source]` policy, or another
/// table a case needs beyond what every fixture already carries.
pub(crate) fn laid_out_workspace(
    files: &[(&str, &str)],
    extra_toml: &str,
) -> TestResult<tempfile::TempDir> {
    let request_bounds = format!(
        "readiness_timeout = \"{}\"\nworker_queue_timeout = \"{}\"\n",
        String::from(FIXTURE_READINESS_TIMEOUT),
        String::from(FIXTURE_WORKER_QUEUE_TIMEOUT)
    );
    fixture_workspace(files, &request_bounds, extra_toml)
}

/// The fixture [`laid_out_workspace`] lays out, with `server_keys` in its
/// `[server]` table ahead of `extra_toml`.
fn fixture_workspace(
    files: &[(&str, &str)],
    server_keys: &str,
    extra_toml: &str,
) -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    for (name, source) in files {
        let target = directory.path().join(name);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, source)?;
    }
    fs::write(
        directory.path().join("rift.toml"),
        format!("{VECTOR_DISABLED}[server]\nidle_timeout = \"60s\"\n{server_keys}{extra_toml}"),
    )?;
    Ok(directory)
}

/// The stderr filter every `rift` child the harness starts runs under.
///
/// Each child gets it explicitly and never inherits `RUST_LOG`: an exported
/// `RUST_LOG=warn` once hid the lifecycle lines a case asserts on (#479).
pub(crate) const CHILD_LOG_FILTER: &str = "rift=info,rift_mcp=info,rift_server=info";

/// Sets [`CHILD_LOG_FILTER`] and `NO_COLOR` on one `rift` child: the traced lines
/// end up in a test's captured output rather than on a terminal, so they stay plain.
pub(crate) fn with_child_log_variables(
    command: &mut std::process::Command,
) -> &mut std::process::Command {
    command
        .env("RUST_LOG", CHILD_LOG_FILTER)
        .env("NO_COLOR", "1")
}

/// Stops the fixture's server when a test unwinds, best effort.
///
/// The stop's standard error reaches the test's own, so a stop that refuses or
/// runs out its window says why in the case's report; its standard output, the
/// one line a stop that worked prints, does not.
pub(crate) struct StopOnDrop {
    root: PathBuf,
    binary: PathBuf,
}

impl StopOnDrop {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            binary: rift_binary(),
        }
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let mut command = std::process::Command::new(&self.binary);
        let _ = with_child_log_variables(&mut command)
            .args(["server", "stop"])
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status();
    }
}

/// Most log records a failure window prints for one workspace: the newest of the window.
const WINDOW_RECORDS_MAX: usize = 200;
/// Most metric snapshot records a failure window prints for one workspace: the newest of
/// the window, a sampler tick or two of every snapshot group.
const WINDOW_SNAPSHOT_RECORDS_MAX: usize = 12;
/// Bytes of one source a failure window prints: its last bytes, the earlier ones cut.
const WINDOW_SOURCE_BYTES_MAX: u64 = 64 << 10;
/// Launch time one `rift server logs` read of a failure window is allowed beside its
/// store waits.
///
/// Measured: while the linker wrote test binaries beside it, the debug `rift` took 5.36 s
/// for a read of a store no process held, and `rift --version`, which opens no store, 3.46
/// s; unloaded, the same read takes 0.03 s to 0.3 s.
const WINDOW_LAUNCH_ALLOWANCE: Duration = Duration::from_secs(8);
/// Longest one `rift server logs` read of a failure window waits: its launch, and one
/// [`rift_tracing::METRICS_BUSY_TIMEOUT_MS`] for each statement of the read that can meet
/// another connection's lock on `.rift/metrics` (the schema version read and the page
/// query). The command installs no log sink, so it waits for no settlement of a server.
const WINDOW_READ_SHARE: Duration = WINDOW_LAUNCH_ALLOWANCE.saturating_add(Duration::from_millis(
    2 * rift_tracing::METRICS_BUSY_TIMEOUT_MS,
));
/// Longest every `rift server logs` read of one failure window waits, together.
///
/// It counts toward the failing case's nextest deadline. The log records of every
/// workspace are read first, each with up to [`WINDOW_READ_SHARE`]; the snapshot reads and
/// the operations in flight scan get what is left, so a slow read can cut only the
/// optional parts. A read the budget cuts says how long it ran and what it printed.
pub(crate) const WINDOW_READ_MAX: Duration = Duration::from_secs(12);
/// Pause between polls of one record read; [`WINDOW_READ_SHARE`] over it bounds the polls.
const WINDOW_READ_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// What the message of every record of the table of operations in flight opens with:
/// the published table, and the stall report past `[logs] stall_delay`.
const IN_FLIGHT_MESSAGE: &str = "operations in flight";
/// The nextest report directory below the workspace root: `[store] dir` of
/// `.config/nextest.toml`, which holds one directory per profile.
const REPORT_DIRECTORY: &str = "target/nextest";
/// The directory, in the report directory of a profile, holding the start of every
/// failure window still open.
const OPEN_WINDOWS_DIRECTORY: &str = "failure-windows";
/// Extension of the file one open failure window's start is written to.
const START_FILE_EXTENSION: &str = "window";
/// Most windows [`print_ended_windows`] prints in one run; each spends up to
/// [`WINDOW_READ_MAX`], and the rest are named and left for the next run.
#[allow(
    dead_code,
    reason = "`server_cli` alone prints ended windows; each suite compiles this file"
)]
const ENDED_WINDOWS_MAX: usize = 8;

/// What the tested processes recorded from the start of one test to its failure,
/// printed on the test's stderr when the test fails and never when it passes.
///
/// The window holds the test's identity and, for each workspace it covers, the
/// server's persisted records from the test's start to the failure, read through the
/// window query of `rift server logs` (`--since`, `--until`, `--kind`): the newest
/// [`WINDOW_RECORDS_MAX`] log records, the newest record of the table of operations in
/// flight, and the newest [`WINDOW_SNAPSHOT_RECORDS_MAX`] metric snapshot records; then
/// the detached server's `.rift/server.stderr`. The stderr of a `rift mcp` child is
/// relayed onto the test's own as it arrives ([`RelayedStderr`]), so it already sits
/// above the window. Each source prints at most [`WINDOW_SOURCE_BYTES_MAX`] bytes and
/// says once what its bound cut, and all reads together wait at most
/// [`WINDOW_READ_MAX`], so one workspace prints at most three sources of that size
/// and one record; a source that could not be read says why, beside the failure and
/// never in its place. Each read prints how long it ran.
///
/// A case begins the window right after its [`StopOnDrop`], so the window drops
/// first and prints before the teardown stop, which can itself outlast nextest's
/// deadline. The case calls [`FailureWindow::passed`] as its last step; an early
/// return through `?` or a panic leaves the window open, and its drop prints it.
///
/// Under nextest the window writes its start, identity, and workspaces into the
/// report directory when it begins, and removes the file when it closes. A test that
/// nextest ends at its deadline, or any other kill, runs no destructor and leaves the
/// file; [`print_ended_windows`] prints those windows after the run.
pub(crate) struct FailureWindow {
    /// The test's name and its nextest identity.
    test: String,
    /// The workspaces whose stores and stderr files the window reads.
    roots: Vec<PathBuf>,
    /// When the test began, on the clock the server stamps its records with.
    started_at: jiff::Timestamp,
    /// When the test began, for the age the heading prints; absent once the test's
    /// process is gone.
    began: Option<std::time::Instant>,
    /// The file the start was written to, nothing outside nextest, or why the write failed.
    start_file: Result<Option<PathBuf>, String>,
    open: bool,
}

impl FailureWindow {
    /// Opens the window of one test serving `root`, before the test starts a process.
    pub(crate) fn begin(root: &Path) -> Self {
        Self::begin_over(&[root])
    }

    /// Opens the window of one test serving every workspace of `roots`, before the
    /// test starts a process.
    pub(crate) fn begin_over(roots: &[&Path]) -> Self {
        let mut window = Self {
            test: test_identity(),
            roots: roots.iter().map(|root| root.to_path_buf()).collect(),
            started_at: jiff::Timestamp::now(),
            began: Some(std::time::Instant::now()),
            start_file: Ok(None),
            open: true,
        };
        window.start_file = window.write_start();
        window
    }

    /// Closes the window of a test that passed: it prints nothing.
    pub(crate) fn passed(mut self) {
        self.open = false;
    }

    /// Writes this window's start into the report directory of the running nextest
    /// profile, and names the file; outside nextest it writes nothing.
    fn write_start(&self) -> Result<Option<PathBuf>, String> {
        let (Ok(workspace), Ok(profile), Ok(attempt)) = (
            std::env::var("NEXTEST_WORKSPACE_ROOT"),
            std::env::var("NEXTEST_PROFILE"),
            std::env::var("NEXTEST_ATTEMPT_ID"),
        ) else {
            return Ok(None);
        };
        let directory = Path::new(&workspace)
            .join(REPORT_DIRECTORY)
            .join(profile)
            .join(OPEN_WINDOWS_DIRECTORY);
        let path = directory.join(format!("{}.{START_FILE_EXTENSION}", file_name_of(&attempt)));
        let mut text = format!("test={}\nstarted_at={}\n", self.test, self.started_at);
        for root in &self.roots {
            text.push_str("root=");
            text.push_str(&root.display().to_string());
            text.push('\n');
        }
        fs::create_dir_all(&directory)
            .and_then(|()| fs::write(&path, text))
            .map(|()| Some(path.clone()))
            .map_err(|error| format!("{}: {error}", path.display()))
    }

    /// The window a start file names, for a test whose process is gone.
    #[allow(
        dead_code,
        reason = "`server_cli` alone prints ended windows; each suite compiles this file"
    )]
    fn restored(path: &Path) -> TestResult<Self> {
        let text = fs::read_to_string(path)?;
        let mut identity = None;
        let mut started_at = None;
        let mut roots = Vec::new();
        for line in text.lines() {
            match line.split_once('=') {
                Some(("test", value)) => identity = Some(value.to_owned()),
                Some(("started_at", value)) => started_at = Some(value.parse()?),
                Some(("root", value)) => roots.push(PathBuf::from(value)),
                _ => return Err(format!("{}: unexpected line {line:?}", path.display()).into()),
            }
        }
        Ok(Self {
            test: identity.ok_or_else(|| format!("{}: no test line", path.display()))?,
            roots,
            started_at: started_at
                .ok_or_else(|| format!("{}: no started_at line", path.display()))?,
            began: None,
            start_file: Ok(Some(path.to_owned())),
            open: true,
        })
    }

    /// The window's text, every source read now: the window ends at this call.
    fn text(&self) -> String {
        let budget = ReadBudget::new();
        let since = millisecond_text(self.started_at.as_millisecond());
        // The query's upper bound excludes its own millisecond; the next one keeps it.
        let until = millisecond_text(jiff::Timestamp::now().as_millisecond().saturating_add(1));
        let age = self.began.map_or_else(
            || "the test's process ended before its window printed".to_owned(),
            |began| format!("{:?} after the test began", began.elapsed()),
        );
        let mut text = format!(
            "\n==== failure window: {test}, {age} ====\nfrom {since} to {until}\n",
            test = self.test,
        );
        if let Err(error) = &self.start_file {
            let _ = writeln!(
                text,
                "[the start was not written ({error}): a kill of this test leaves no window]"
            );
        }
        let mut workspaces: Vec<WorkspaceReads> = self
            .roots
            .iter()
            .map(|root| WorkspaceReads::records(root, &since, &until, &budget))
            .collect();
        for workspace in &mut workspaces {
            workspace.read_snapshots(&since, &until, &budget);
        }
        for workspace in &mut workspaces {
            workspace.scan_in_flight(&since, &until, &budget);
        }
        for workspace in &workspaces {
            text.push_str(&workspace.text());
        }
        text.push_str(
            "---- rift mcp stderr: relayed above as it arrived ----\n\
             ==== end of failure window ====\n",
        );
        text
    }
}

impl Drop for FailureWindow {
    fn drop(&mut self) {
        if self.open {
            let _ = std::io::stderr().write_all(self.text().as_bytes());
        }
        if let Ok(Some(path)) = &self.start_file {
            let _ = fs::remove_file(path);
        }
    }
}

/// The test's name and the nextest variables that identify its run and attempt.
fn test_identity() -> String {
    let test = std::thread::current()
        .name()
        .unwrap_or("unnamed test")
        .to_owned();
    let identity: Vec<String> = ["NEXTEST_BINARY_ID", "NEXTEST_ATTEMPT_ID"]
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| format!("{name}={value}"))
        })
        .collect();
    format!("{test} {}", identity.join(" "))
}

/// `attempt` with every character a file name may not carry on some platform replaced.
fn file_name_of(attempt: &str) -> String {
    attempt
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// `milliseconds` since the Unix epoch in RFC 3339, the form `--since` and `--until` take.
fn millisecond_text(milliseconds: i64) -> String {
    jiff::Timestamp::from_millisecond(milliseconds).map_or_else(
        |error| format!("invalid instant: {error}"),
        |at| at.to_string(),
    )
}

/// Prints the failure window of every test of the compiled binary whose process
/// ended before its own window printed - a nextest timeout or another kill - from the
/// start files left in the report directory of every profile, and removes each file it
/// printed. Answers how many it printed. At most [`ENDED_WINDOWS_MAX`] print; the rest
/// are named, and kept for the next run.
///
/// A window printed here ends when this run reads it: the test's own end is unknown.
/// Run it after nextest returns, never beside a running test, whose open window it
/// would print and remove.
#[allow(
    dead_code,
    reason = "`server_cli` alone prints ended windows; each suite compiles this file"
)]
pub(crate) fn print_ended_windows() -> TestResult<usize> {
    let workspace = std::env::var("NEXTEST_WORKSPACE_ROOT")
        .map_err(|_| "run under nextest, which names the workspace root")?;
    let reports = Path::new(&workspace).join(REPORT_DIRECTORY);
    let mut starts = Vec::new();
    if let Ok(profiles) = fs::read_dir(&reports) {
        for profile in profiles {
            let directory = profile?.path().join(OPEN_WINDOWS_DIRECTORY);
            let Ok(files) = fs::read_dir(&directory) else {
                continue;
            };
            for file in files {
                let path = file?.path();
                if path.extension() == Some(START_FILE_EXTENSION.as_ref()) {
                    starts.push(path);
                }
            }
        }
    }
    starts.sort();
    let printed = starts.len().min(ENDED_WINDOWS_MAX);
    for start in &starts[..printed] {
        // The restored window is open: dropping it prints it and removes its start.
        drop(FailureWindow::restored(start)?);
    }
    for kept in &starts[printed..] {
        let _ = writeln!(
            std::io::stderr(),
            "[not printed, past the {ENDED_WINDOWS_MAX}-window bound of one run: {}]",
            kept.display()
        );
    }
    Ok(printed)
}

/// The [`WINDOW_READ_MAX`] budget every read of one failure window shares.
struct ReadBudget {
    deadline: std::time::Instant,
}

impl ReadBudget {
    fn new() -> Self {
        Self {
            deadline: std::time::Instant::now() + WINDOW_READ_MAX,
        }
    }

    /// The bound of the next read: [`WINDOW_READ_SHARE`], or what is left of the budget.
    fn next_bound(&self) -> Result<Duration, String> {
        let left = self
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(format!(
                "not run: the window's {WINDOW_READ_MAX:?} read budget was spent by the reads \
                 above"
            ));
        }
        Ok(left.min(WINDOW_READ_SHARE))
    }
}

/// The newest record of the table of operations in flight of one workspace's window.
enum InFlight {
    /// The record, as printed.
    Found(String),
    /// No such record in the window.
    Absent,
    /// The bounded log read cut older records; a scan of every record decides.
    Cut,
    /// Why it could not be read.
    NotRead(String),
}

/// What the reads of a failure window found for one workspace.
struct WorkspaceReads {
    root: PathBuf,
    /// The log records of the window, under the command line that read them.
    records: String,
    in_flight: InFlight,
    /// The metric snapshot records of the window, under the command line that read them.
    snapshots: String,
}

impl WorkspaceReads {
    /// Reads the newest [`WINDOW_RECORDS_MAX`] log records of the window in `root`.
    fn records(root: &Path, since: &str, until: &str, budget: &ReadBudget) -> Self {
        let tail = WINDOW_RECORDS_MAX.to_string();
        let arguments = window_arguments(since, until, "log", &tail);
        let mut records = read_heading(&arguments);
        let in_flight = match read_window(root, &arguments, budget) {
            Ok(read) => {
                let count = read.printed.lines().filter(|line| !line.is_empty()).count();
                let newest = newest_in_flight(read.printed.lines());
                records.push_str(&bounded_tail(&read.printed));
                records.push_str(&read.took);
                if count >= WINDOW_RECORDS_MAX {
                    let _ = writeln!(
                        records,
                        "[the read reached its {WINDOW_RECORDS_MAX}-record bound; older records \
                         of the window are cut]"
                    );
                }
                match newest {
                    Some(line) => InFlight::Found(line),
                    None if count >= WINDOW_RECORDS_MAX => InFlight::Cut,
                    None => InFlight::Absent,
                }
            }
            Err(error) => {
                let _ = writeln!(records, "the record read did not finish: {error}");
                InFlight::NotRead(format!("the record read did not finish: {error}"))
            }
        };
        Self {
            root: root.to_owned(),
            records,
            in_flight,
            snapshots: String::new(),
        }
    }

    /// Reads the newest [`WINDOW_SNAPSHOT_RECORDS_MAX`] metric snapshot records of the window.
    fn read_snapshots(&mut self, since: &str, until: &str, budget: &ReadBudget) {
        let tail = WINDOW_SNAPSHOT_RECORDS_MAX.to_string();
        let arguments = window_arguments(since, until, "metric", &tail);
        self.snapshots = read_heading(&arguments);
        match read_window(&self.root, &arguments, budget) {
            Ok(read) => {
                let count = read.printed.lines().filter(|line| !line.is_empty()).count();
                self.snapshots.push_str(&bounded_tail(&read.printed));
                self.snapshots.push_str(&read.took);
                if count >= WINDOW_SNAPSHOT_RECORDS_MAX {
                    let _ = writeln!(
                        self.snapshots,
                        "[the newest {WINDOW_SNAPSHOT_RECORDS_MAX} snapshot records of the \
                         window; older ones are not read]"
                    );
                }
            }
            Err(error) => {
                let _ = writeln!(self.snapshots, "the snapshot read did not finish: {error}");
            }
        }
    }

    /// Scans every log record of the window for the newest record of the table of
    /// operations in flight, when the bounded read cut older records.
    fn scan_in_flight(&mut self, since: &str, until: &str, budget: &ReadBudget) {
        use std::io::BufRead as _;

        if !matches!(self.in_flight, InFlight::Cut) {
            return;
        }
        let arguments = window_arguments(since, until, "log", "all");
        let scanned = run_window_read(&self.root, &arguments, budget).and_then(|(file, _)| {
            let mut newest = None;
            for line in std::io::BufReader::new(file).lines() {
                let line = line.map_err(|error| format!("could not read the read: {error}"))?;
                if line.contains(IN_FLIGHT_MESSAGE) {
                    newest = Some(line);
                }
            }
            Ok(newest)
        });
        self.in_flight = match scanned {
            Ok(Some(line)) => InFlight::Found(line),
            Ok(None) => InFlight::Absent,
            Err(error) => InFlight::NotRead(error),
        };
    }

    /// The part of a failure window this workspace holds: its records of the window, then
    /// its server's stderr file.
    fn text(&self) -> String {
        let mut text = format!("---- workspace {} ----\n", self.root.display());
        text.push_str(&self.records);
        text.push_str("---- newest record of the operations in flight in the window ----\n");
        match &self.in_flight {
            InFlight::Found(line) => {
                text.push_str(&bounded_tail(line));
                text.push('\n');
            }
            InFlight::Absent => text.push_str("none recorded in the window\n"),
            InFlight::Cut => text.push_str("not scanned\n"),
            InFlight::NotRead(error) => {
                let _ = writeln!(text, "not read: {error}");
            }
        }
        text.push_str(&self.snapshots);
        let stderr_file = rift_mcp::stderr_file_path(&self.root);
        let _ = writeln!(
            text,
            "---- {} ----\n{}",
            stderr_file.display(),
            file_tail(&stderr_file)
        );
        text
    }
}

/// The window query arguments of one read: `--since`, `--until`, `--kind`, `--tail`.
fn window_arguments<'a>(
    since: &'a str,
    until: &'a str,
    kind: &'a str,
    tail: &'a str,
) -> [&'a str; 8] {
    [
        "--since", since, "--until", until, "--kind", kind, "--tail", tail,
    ]
}

/// The heading of one record read: the command line a person runs to read it again.
fn read_heading(arguments: &[&str]) -> String {
    format!("---- rift server logs {} ----\n", arguments.join(" "))
}

/// The last line of `lines` that a record of the table of operations in flight printed.
fn newest_in_flight<'a>(lines: impl Iterator<Item = &'a str>) -> Option<String> {
    lines
        .filter(|line| line.contains(IN_FLIGHT_MESSAGE))
        .last()
        .map(str::to_owned)
}

/// What one finished read printed, and the line stating how long it ran.
struct WindowRead {
    printed: String,
    took: String,
}

/// What one `rift server logs <arguments>` run printed in `root`, stdout then stderr.
fn read_window(root: &Path, arguments: &[&str], budget: &ReadBudget) -> Result<WindowRead, String> {
    let (mut file, took) = run_window_read(root, arguments, budget)?;
    let mut printed = String::new();
    file.read_to_string(&mut printed)
        .map_err(|error| format!("could not read what it printed: {error}"))?;
    Ok(WindowRead { printed, took })
}

/// Runs `rift server logs <arguments>` in `root` until it exits or the bound the budget
/// gives it passes, and answers the file holding what it printed, rewound, and the line
/// stating how long it ran and, when it failed, its exit status.
///
/// Both streams land in one anonymous file rather than a pipe, so a long print never
/// blocks the command while the window polls it. A read the bound ends is killed, and
/// its refusal names how long it ran and how many bytes it printed: none means it was
/// still launching or opening the store.
fn run_window_read(
    root: &Path,
    arguments: &[&str],
    budget: &ReadBudget,
) -> Result<(fs::File, String), String> {
    let bound = budget.next_bound()?;
    let failed = |error: std::io::Error| format!("could not run the read: {error}");
    let mut printed = tempfile::tempfile().map_err(failed)?;
    let mut command = std::process::Command::new(rift_binary());
    let started = std::time::Instant::now();
    let mut child = with_child_log_variables(&mut command)
        .args(["server", "logs"])
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(printed.try_clone().map_err(failed)?)
        .stderr(printed.try_clone().map_err(failed)?)
        .spawn()
        .map_err(failed)?;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(failed)? {
            break status;
        }
        if started.elapsed() >= bound {
            let _ = child.kill();
            let _ = child.wait();
            let bytes = printed.metadata().map_or(0, |metadata| metadata.len());
            return Err(format!(
                "rift server logs ran {:?} and was ended at its {bound:?} bound, having printed \
                 {bytes} bytes",
                started.elapsed()
            ));
        }
        std::thread::sleep(WINDOW_READ_POLL_INTERVAL);
    };
    let mut took = format!("[read in {:?}", started.elapsed());
    if !status.success() {
        let _ = write!(took, ", exited with {status}");
    }
    took.push_str("]\n");
    printed.rewind().map_err(failed)?;
    Ok((printed, took))
}

/// The last [`WINDOW_SOURCE_BYTES_MAX`] bytes of the file at `path`, or why it could
/// not be read.
fn file_tail(path: &Path) -> String {
    match fs::File::open(path) {
        Ok(mut file) => tail_of(&mut file).unwrap_or_else(|error| format!("unreadable: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            "absent: only a server `rift server start` spawned writes this file".to_owned()
        }
        Err(error) => format!("unreadable: {error}"),
    }
}

/// The last [`WINDOW_SOURCE_BYTES_MAX`] bytes of `file`, preceded by a notice of the
/// bytes before them that the bound cut.
fn tail_of(file: &mut fs::File) -> std::io::Result<String> {
    let length = file.metadata()?.len();
    let cut = length.saturating_sub(WINDOW_SOURCE_BYTES_MAX);
    file.seek(std::io::SeekFrom::Start(cut))?;
    let mut kept = Vec::new();
    file.take(WINDOW_SOURCE_BYTES_MAX).read_to_end(&mut kept)?;
    Ok(format!(
        "{}{}",
        cut_notice(cut),
        String::from_utf8_lossy(&kept)
    ))
}

/// The last [`WINDOW_SOURCE_BYTES_MAX`] bytes of `text`, preceded by a notice of the
/// bytes before them that the bound cut, for retained stderr a failure message carries.
pub(crate) fn bounded_tail(text: &str) -> String {
    let bytes = text.as_bytes();
    let kept = usize::try_from(WINDOW_SOURCE_BYTES_MAX).unwrap_or(usize::MAX);
    let cut = bytes.len().saturating_sub(kept);
    format!(
        "{}{}",
        cut_notice(u64::try_from(cut).unwrap_or(u64::MAX)),
        String::from_utf8_lossy(&bytes[cut..])
    )
}

/// The one line saying how many leading bytes a source's bound cut, or nothing.
fn cut_notice(cut: u64) -> String {
    if cut == 0 {
        return String::new();
    }
    format!("[the first {cut} bytes are cut by the {WINDOW_SOURCE_BYTES_MAX}-byte bound]\n")
}

/// Runs the real binary with `arguments` inside the fixture workspace,
/// off the async runtime.
pub(crate) async fn run_rift(root: &Path, arguments: &[&str]) -> TestResult<std::process::Output> {
    let root = root.to_owned();
    let arguments: Vec<String> = arguments.iter().map(|&argument| argument.into()).collect();
    let output = tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(rift_binary());
        with_child_log_variables(&mut command)
            .args(&arguments)
            .current_dir(&root)
            .stdin(Stdio::null())
            .output()
    })
    .await??;
    Ok(output)
}

pub(crate) fn require_success(output: &std::process::Output, what: &str) -> TestResult {
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{what} must succeed: status {:?}, stdout {:?}, stderr {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
    .into())
}

/// Bounds one proxied operation by [`PROXIED_CALL_MAX`].
pub(crate) async fn within<Value>(
    what: &str,
    operation: impl Future<Output = Value>,
) -> TestResult<Value> {
    tokio::time::timeout(PROXIED_CALL_MAX, operation)
        .await
        .map_err(|_elapsed| format!("timed out waiting for {what}").into())
}

/// The base `rift mcp` child command for one fixture workspace, before either
/// the rmcp transport wrapper or a raw-pipe session spawns it.
///
/// The child runs under [`with_child_log_variables`]; the server the proxy
/// spawns inherits both variables. `arguments` follow the `mcp` subcommand.
fn base_command(root: &Path, arguments: &[&str]) -> tokio::process::Command {
    let mut command = std::process::Command::new(rift_binary());
    with_child_log_variables(&mut command)
        .arg("mcp")
        .args(arguments)
        .current_dir(root);
    tokio::process::Command::from(command)
}

/// The `rift mcp` child command for one fixture workspace.
fn proxy_command(root: &Path, arguments: &[&str]) -> TokioChildProcessBuilder {
    TokioChildProcess::builder(base_command(root, arguments))
}

/// One connected `rift mcp` child whose stderr is relayed onto the test's own.
pub(crate) async fn proxy_client(root: &Path) -> TestResult<RunningService<RoleClient, ()>> {
    proxy_client_with(root, &[]).await
}

/// One connected `rift mcp` child started with `arguments` after the
/// subcommand, such as `--output=text`; its stderr is relayed like
/// [`proxy_client`]'s.
pub(crate) async fn proxy_client_with(
    root: &Path,
    arguments: &[&str],
) -> TestResult<RunningService<RoleClient, ()>> {
    let (client, _stderr) = relayed_proxy_client_with(root, arguments).await?;
    Ok(client)
}

/// One connected `rift mcp` child, and its stderr relayed onto the test's own.
///
/// The proxy's lifecycle lines, and the stderr of a server it spawned that
/// exited before serving, are what explain a start that never answered, so
/// every proxied case keeps them where a failure report shows them.
pub(crate) async fn relayed_proxy_client(
    root: &Path,
) -> TestResult<(RunningService<RoleClient, ()>, RelayedStderr)> {
    relayed_proxy_client_with(root, &[]).await
}

/// [`relayed_proxy_client`] with `arguments` after the `mcp` subcommand.
async fn relayed_proxy_client_with(
    root: &Path,
    arguments: &[&str],
) -> TestResult<(RunningService<RoleClient, ()>, RelayedStderr)> {
    let (reader, writer) = std::io::pipe()?;
    let (transport, _stderr) = proxy_command(root, arguments).stderr(writer).spawn()?;
    let stderr = RelayedStderr::spawn(reader);
    Ok((().serve(transport).await?, stderr))
}

/// Bytes of one child's stderr the relay writes and keeps; the rest is read
/// and dropped, so the child never blocks on a full pipe.
const RELAYED_STDERR_BYTES_MAX: usize = 1 << 20;

/// A child's stderr, copied onto this test process's stderr as it arrives
/// and kept for the test to assert on.
///
/// Nextest prints a test's captured output when the test fails or times
/// out, and it ends a timed-out test by killing it - on Windows at once,
/// with every descendant - so a copy printed after the fact would never
/// run. The relay writes each read as it lands, through `std::io::stderr`,
/// which no test harness capture intercepts.
pub(crate) struct RelayedStderr {
    bytes: Arc<Mutex<Vec<u8>>>,
    relay: std::thread::JoinHandle<()>,
}

impl RelayedStderr {
    /// Starts relaying `stream` on a thread of its own, which ends at the
    /// stream's end-of-file.
    ///
    /// The relay stays off tokio's blocking pool: the runtime's shutdown
    /// joins every blocking thread, so a read parked there would hold the
    /// test's end until the stream closed.
    fn spawn(stream: impl Read + Send + 'static) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&bytes);
        Self {
            bytes,
            relay: std::thread::spawn(move || relay_until_closed(stream, &captured)),
        }
    }

    /// Standard error retained so far, before the child closes its stream.
    pub(crate) fn snapshot(&self) -> String {
        let retained = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&retained).into_owned()
    }

    /// The relayed text, once every holder of the stream's write end has
    /// closed it.
    ///
    /// On Windows a detached server the child started inherits that end,
    /// so the text arrives only when the server leaves too.
    pub(crate) async fn text(self) -> TestResult<String> {
        let Self { bytes, relay } = self;
        tokio::task::spawn_blocking(move || relay.join())
            .await?
            .map_err(|_panic| "the stderr relay panicked")?;
        let retained = bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(String::from_utf8_lossy(&retained).into_owned())
    }
}

/// Copies `stream` onto this process's stderr until end-of-file, keeping
/// what it copied, both bounded by [`RELAYED_STDERR_BYTES_MAX`].
///
/// The first read the bound cuts writes one notice after the relayed bytes;
/// the notice is not retained, so a case's assertions see the child's bytes alone.
fn relay_until_closed(mut stream: impl Read, captured: &Mutex<Vec<u8>>) {
    let mut buffer = [0_u8; rift_core::STREAM_READ_BYTES];
    let mut cut_reported = false;
    loop {
        let read_bytes = match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read_bytes) => read_bytes,
        };
        let relayed = {
            let mut retained = captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let room = RELAYED_STDERR_BYTES_MAX.saturating_sub(retained.len());
            let relayed = &buffer[..read_bytes.min(room)];
            retained.extend_from_slice(relayed);
            relayed
        };
        let _ = std::io::stderr().write_all(relayed);
        if relayed.len() < read_bytes && !cut_reported {
            cut_reported = true;
            let _ = writeln!(
                std::io::stderr(),
                "\n[relayed stderr reached its {RELAYED_STDERR_BYTES_MAX}-byte bound; the rest \
                 is read and dropped]"
            );
        }
    }
}

pub(crate) fn arguments(
    value: &serde_json::Value,
) -> TestResult<serde_json::Map<String, serde_json::Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "tool arguments must be an object".into())
}

/// Most attempts one proxied call spends on a retryable refusal.
pub(crate) const ACCEPTANCE_ATTEMPTS_MAX: usize = 8;

/// One proxied tool call returning its structured result, retrying the
/// refusal the server advertises as `retry: same_request`: a source
/// change moves the index, and a request whose snapshot predates the change
/// is refused rather than served stale.
pub(crate) async fn proxied_call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    call_arguments: &serde_json::Value,
) -> TestResult<serde_json::Value> {
    proxied_call_within(client, name, call_arguments, PROXIED_CALL_MAX).await
}

/// One proxied tool call returning the whole result, with the same retry as
/// [`proxied_call`]; `content` and `structured_content` are the caller's to
/// read. A failure that is not retryable comes back as its error result.
pub(crate) async fn proxied_result(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    call_arguments: &serde_json::Value,
) -> TestResult<CallToolResult> {
    proxied_result_within(client, name, call_arguments, PROXIED_CALL_MAX).await
}

/// MIME type of the compact text content a resource read returns first.
const RESOURCE_TEXT_MIME: &str = "text/plain";
/// MIME type of the JSON body a resource read returns second.
const RESOURCE_JSON_MIME: &str = "application/json";

/// The `(mime type, text)` of every content of a resource read; each must be text and carry
/// the requested `uri`.
fn resource_texts<'a>(
    answer: &'a ReadResourceResult,
    uri: &str,
) -> TestResult<Vec<(&'a str, &'a str)>> {
    let mut texts = Vec::with_capacity(answer.contents.len());
    for content in &answer.contents {
        let ResourceContents::TextResourceContents {
            uri: found,
            mime_type,
            text,
            ..
        } = content
        else {
            return Err(format!("{uri}: every content must be text: {content:?}").into());
        };
        if found != uri {
            return Err(format!("{uri}: a content carries uri {found}").into());
        }
        texts.push((mime_type.as_deref().unwrap_or_default(), text.as_str()));
    }
    Ok(texts)
}

/// The JSON body of a resource read.
///
/// Requires exactly one `application/json` content; when two contents arrive, the other is
/// `text/plain`. Every content carries `uri`.
pub(crate) fn resource_json(
    answer: &ReadResourceResult,
    uri: &str,
) -> TestResult<serde_json::Value> {
    let texts = resource_texts(answer, uri)?;
    let (json, others): (Vec<_>, Vec<_>) = texts
        .iter()
        .partition(|(mime, _)| *mime == RESOURCE_JSON_MIME);
    let [(_, body)] = json.as_slice() else {
        return Err(
            format!("{uri}: want exactly one {RESOURCE_JSON_MIME} content: {texts:?}").into(),
        );
    };
    if others.len() > 1 || others.iter().any(|(mime, _)| *mime != RESOURCE_TEXT_MIME) {
        return Err(
            format!("{uri}: the other content must be {RESOURCE_TEXT_MIME}: {texts:?}").into(),
        );
    }
    Ok(serde_json::from_str(body)?)
}

/// The compact text of a resource read: its first content, which must be non-empty
/// `text/plain` carrying `uri`.
pub(crate) fn resource_text<'a>(answer: &'a ReadResourceResult, uri: &str) -> TestResult<&'a str> {
    match resource_texts(answer, uri)?.first() {
        Some((mime, text)) if *mime == RESOURCE_TEXT_MIME && !text.is_empty() => Ok(*text),
        other => Err(format!(
            "{uri}: the first content must be non-empty {RESOURCE_TEXT_MIME}: {other:?}"
        )
        .into()),
    }
}

/// Reads map until workspace file preparation finishes, within fixture bound.
pub(crate) async fn await_workspace_ready(
    client: &RunningService<RoleClient, ()>,
) -> TestResult<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let answer = tokio::time::timeout_at(
            deadline,
            client.read_resource(ReadResourceRequestParams::new("rift://map".to_owned())),
        )
        .await??;
        let body = resource_json(&answer, "rift://map")?;
        let preparing = body["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|warning| warning["code"] == "local_index_preparing");
        if !preparing {
            return Ok(body);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("workspace map remained in preparation: {body}").into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Reads map until workspace file preparation finishes, judging by its compact text.
///
/// The text content arrives through a proxy of any `--output` mode, so this serves a
/// `text` proxy too, which carries no JSON body. A warning line is `  <code>` (2 spaces)
/// followed by a space, a colon, or the end of the line.
pub(crate) async fn await_workspace_text(
    client: &RunningService<RoleClient, ()>,
) -> TestResult<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let answer = tokio::time::timeout_at(
            deadline,
            client.read_resource(ReadResourceRequestParams::new("rift://map".to_owned())),
        )
        .await??;
        let text = resource_text(&answer, "rift://map")?;
        if !text.starts_with("map ") {
            return Err(format!("the map text must open with `map `: {text}").into());
        }
        let preparing = text.lines().any(|line| {
            line.strip_prefix("\tlocal_index_preparing")
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', ':']))
        });
        if !preparing {
            return Ok(text.to_owned());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("workspace map remained in preparation: {text}").into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// One proxied live-engine call under [`PROXIED_ENGINE_CALL_MAX`].
pub(crate) async fn proxied_engine_call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    call_arguments: &serde_json::Value,
) -> TestResult<serde_json::Value> {
    proxied_call_within(client, name, call_arguments, PROXIED_ENGINE_CALL_MAX).await
}

/// One retrying proxied call under its caller-owned wall-clock bound.
async fn proxied_call_within(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    call_arguments: &serde_json::Value,
    timeout: Duration,
) -> TestResult<serde_json::Value> {
    let called = proxied_result_within(client, name, call_arguments, timeout).await?;
    if called.is_error == Some(true) {
        return Err(format!("{name} failed: {}", tool_failure(&called)?.text).into());
    }
    called
        .structured_content
        .ok_or_else(|| format!("{name} must return structured content").into())
}

/// One retrying proxied call returning the whole result under its caller-owned
/// wall-clock bound.
async fn proxied_result_within(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    call_arguments: &serde_json::Value,
    timeout: Duration,
) -> TestResult<CallToolResult> {
    for _attempt in 0..ACCEPTANCE_ATTEMPTS_MAX {
        let params = CallToolRequestParams::new(name).with_arguments(arguments(call_arguments)?);
        let called = match tokio::time::timeout(timeout, client.call_tool(params))
            .await
            .map_err(|_elapsed| format!("timed out waiting for {name}"))?
        {
            Ok(called) => called,
            Err(rmcp::ServiceError::McpError(error))
                if error
                    .data
                    .as_ref()
                    .is_some_and(|data| data.get("retry") == Some(&json!("same_request"))) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if called.is_error != Some(true)
            || tool_failure(&called)?.retry != RetryDirective::SameRequest
        {
            return Ok(called);
        }
    }
    Err(format!("the server kept refusing {name}").into())
}

/// The facts of one failed tool call, read from its error text.
#[derive(Debug)]
pub(crate) struct ToolFailure {
    /// The code of the first entry head.
    pub(crate) code: ErrorCode,
    /// Line 3 as written, without its indent. The text writes control characters as
    /// `\n`, `\r`, `\t` and `\u{HEX}` and leaves backslash as it is, so a Windows path such
    /// as `C:\Users\runneradmin` holds the same `\r`: the line cannot be turned back.
    pub(crate) message: String,
    /// The retry directive of the first entry head.
    pub(crate) retry: RetryDirective,
    /// The whole text block, for assertions on `limit`, cause entry and diagnostic lines.
    pub(crate) text: String,
}

/// Reads the failure of an error result.
///
/// Requires `is_error == Some(true)`, no structured content, and exactly one text block.
/// Line 1 is `N error(s)`; line 2 is the first entry head `  <code> · retry <directive>`;
/// line 3 is its message at 4 spaces, on one line.
pub(crate) fn tool_failure(result: &CallToolResult) -> TestResult<ToolFailure> {
    if result.is_error != Some(true) {
        return Err(format!("is_error must be Some(true), got {:?}", result.is_error).into());
    }
    if result.structured_content.is_some() {
        return Err("an error result must carry no structured content".into());
    }
    let [block] = result.content.as_slice() else {
        return Err(format!("content must be one block, got {:?}", result.content).into());
    };
    let text = block
        .as_text()
        .ok_or("the content block must be text")?
        .text
        .clone();
    let mut lines = text.lines();
    let title = lines.next().ok_or("the error text is empty")?;
    if !failure_title(title) {
        return Err(format!("line 1 must be `N error(s)`: {title}").into());
    }
    let head = lines
        .next()
        .ok_or_else(|| format!("the error text has no entry head: {text}"))?;
    let message = lines
        .next()
        .and_then(|line| line.strip_prefix(FAILURE_LINE_INDENT))
        .ok_or_else(|| format!("line 3 must be the message at two levels: {text}"))?;
    let (code, retry) = failure_head(head)?;
    Ok(ToolFailure {
        code,
        message: message.to_owned(),
        retry,
        text,
    })
}

/// Text of one indent level of an answer text.
const INDENT_UNIT: &str = "\t";
/// Indent of the lines under an entry head of a failure text: two levels.
const FAILURE_LINE_INDENT: &str = "\t\t";

/// Whether `line` is the title of a failure: `N error` or `N errors`.
fn failure_title(line: &str) -> bool {
    line.strip_suffix(" errors")
        .or_else(|| line.strip_suffix(" error"))
        .is_some_and(|count| !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The code and retry directive of the entry head `<code> · retry <directive>`, one level deep.
fn failure_head(line: &str) -> TestResult<(ErrorCode, RetryDirective)> {
    let (code, retry) = line
        .strip_prefix(INDENT_UNIT)
        .filter(|rest| !rest.starts_with(INDENT_UNIT))
        .and_then(|rest| rest.split_once(" · retry "))
        .ok_or_else(|| {
            format!("line 2 must be `<code> · retry <directive>` at one level: {line}")
        })?;
    Ok((
        serde_json::from_value(serde_json::Value::String(code.to_owned()))?,
        serde_json::from_value(serde_json::Value::String(retry.to_owned()))?,
    ))
}
