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

use crate::test_case::with_test_case_name;

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
/// The child also carries the test's [`TEST_CASE_NAME_ATTRIBUTE`]
/// ([`with_test_case_name`]).
pub(crate) fn with_child_log_variables(
    command: &mut std::process::Command,
) -> &mut std::process::Command {
    with_test_case_name(command)
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
/// workspace are read first, each with up to [`WINDOW_READ_SHARE`]; the operations in
/// flight scan gets what is left, so a slow read can cut only the optional part. A read the budget cuts says how long it ran and what it printed.
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
/// Extension of the file a window that ended in its test's own process writes its printed
/// text to.
const TEXT_FILE_EXTENSION: &str = "text";
/// Extension of the file one registered process's stderr is copied to.
const STDERR_FILE_EXTENSION: &str = "stderr";
/// Bytes of one registered process's stderr its window file keeps: the first ones. The
/// bytes past it are read and dropped, one notice line follows the cut, and a line
/// counting the dropped bytes ends the file once the stream closes, so the tail a window
/// prints of a cut file is the bytes before the bound, not the stream's end.
pub(crate) const STDERR_FILE_BYTES_MAX: u64 = 16 << 20;
/// Exit text of a registered process whose exit the test did not observe.
const EXIT_NOT_OBSERVED: &str = "still running at the window, as far as the test observed";
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
/// The window holds the test's identity, `T`, the moment the window ends, and the host
/// facts ([`machine_line`]); then, for each workspace it covers, the
/// server's persisted records from the test's start to the failure, read through the
/// window query of `rift server logs` (`--since`, `--until`): the newest
/// [`WINDOW_RECORDS_MAX`] log records and the newest record of the table of operations in
/// flight; then the detached server's `.rift/server.stderr`. Then, for each process the
/// test registered (a foreground server, a `rift mcp` child), its label, its exit status,
/// and the tail of the copy of its stderr the harness kept beside the start. The stderr of
/// a `rift mcp` child is also relayed onto the test's own as it arrives
/// ([`RelayedStderr`]). Each source prints at most [`WINDOW_SOURCE_BYTES_MAX`] bytes and
/// says once what its bound cut, and all reads together wait at most
/// [`WINDOW_READ_MAX`], so one workspace prints at most two sources of that size
/// and one record; a source that could not be read says why, beside the failure and
/// never in its place. Each read prints how long it ran.
///
/// A window covering one workspace merges the stderr of each registered `rift mcp` child
/// into that workspace's records by timestamp ([`merged_by_timestamp`]). A stderr file
/// holding [`STORE_REFUSAL`] is printed after a line saying the window falls back to it.
///
/// A case begins the window right after its [`StopOnDrop`], so the window drops
/// first and prints before the teardown stop, which can itself outlast nextest's
/// deadline. The case calls [`FailureWindow::passed`] as its last step; an early
/// return through `?` or a panic leaves the window open, and its drop prints it.
///
/// Under nextest the window keeps its files in the report directory ([`WindowFiles`]).
/// It writes its start, identity, and workspaces when it begins; the harness appends
/// each process the test registers and each exit the test observes, and copies each
/// registered process's stderr beside it. A window that closes passed removes every
/// file. A window that fails in the test's own process keeps them, writes its printed
/// text beside them, and appends `ended_at` last. A test that nextest ends at its
/// deadline, or any other kill, runs no destructor and leaves the files without
/// `ended_at`; [`print_ended_windows`] prints those windows after the run.
pub(crate) struct FailureWindow {
    /// The test's name and its nextest identity.
    test: String,
    /// The workspaces whose stores and stderr files the window reads.
    roots: Vec<PathBuf>,
    /// When the test began, on the clock the server stamps its records with.
    started_at: jiff::Timestamp,
    /// When the test began, for the age the heading prints; absent once the test's
    /// process is gone, for a window [`print_ended_windows`] restored.
    began: Option<std::time::Instant>,
    /// The window's files, nothing outside nextest, or why the start was not written.
    files: Result<Option<WindowFiles>, String>,
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
        Self::begin_in(
            WindowFiles::of_this_test(),
            std::env::var("NEXTEST_ATTEMPT_ID").ok().as_deref(),
            roots,
        )
    }

    /// Opens a window whose start `files` hold, of the test attempt `attempt`.
    pub(crate) fn begin_in(
        files: Option<WindowFiles>,
        attempt: Option<&str>,
        roots: &[&Path],
    ) -> Self {
        let mut window = Self {
            test: test_identity(),
            roots: roots.iter().map(|root| root.to_path_buf()).collect(),
            started_at: jiff::Timestamp::now(),
            began: Some(std::time::Instant::now()),
            files: Ok(None),
            open: true,
        };
        window.files = match files {
            Some(files) => window.write_start(&files, attempt).map(|()| Some(files)),
            None => Ok(None),
        };
        window
    }

    /// Closes the window of a test that passed: it prints nothing.
    pub(crate) fn passed(mut self) {
        self.open = false;
    }

    /// Writes this window's start, `test`, `attempt`, `started_at`, and one `root` per
    /// workspace, into `files`.
    fn write_start(&self, files: &WindowFiles, attempt: Option<&str>) -> Result<(), String> {
        let mut text = format!("test={}\n", self.test);
        if let Some(attempt) = attempt {
            let _ = writeln!(text, "attempt={attempt}");
        }
        let _ = writeln!(text, "started_at={}", self.started_at);
        for root in &self.roots {
            let _ = writeln!(text, "root={}", root.display());
        }
        let path = files.start();
        fs::create_dir_all(&files.directory)
            .and_then(|()| fs::write(&path, text))
            .map_err(|error| format!("{}: {error}", path.display()))
    }

    /// The window a start file names, for a test whose process is gone.
    #[allow(
        dead_code,
        reason = "`server_cli` alone prints ended windows; each suite compiles this file"
    )]
    fn restored(path: &Path) -> TestResult<Self> {
        let start = WindowStart::read(path)?;
        Ok(Self {
            test: start.test,
            roots: start.roots,
            started_at: start.started_at,
            began: None,
            files: Ok(Some(WindowFiles::of_start(path)?)),
            open: true,
        })
    }

    /// The window's text, every source read now; the window ends at `ended_at`.
    fn text(&self, ended_at: jiff::Timestamp) -> String {
        let budget = ReadBudget::new();
        let since = millisecond_text(self.started_at.as_millisecond());
        // The query's upper bound excludes its own millisecond; the next one keeps it.
        let until = millisecond_text(ended_at.as_millisecond().saturating_add(1));
        let age = self.began.map_or_else(
            || "the test's process ended before its window printed".to_owned(),
            |began| format!("{:?} after the test began", began.elapsed()),
        );
        let mut text = format!(
            "\n==== failure window: {test}, {age} ====\nfrom {since} to {until}\n",
            test = self.test,
        );
        if let Err(error) = &self.files {
            let _ = writeln!(
                text,
                "[the start was not written ({error}): a kill of this test leaves no window]"
            );
        }
        match self.began {
            Some(_) => {
                let _ = writeln!(text, "T: {ended_at}");
            }
            None => text.push_str("T: not observed: this window ends when the run reads it\n"),
        }
        text.push_str(&machine_line());
        let registered = self.registered();
        // The lines of a `rift mcp` child merge with the store records by timestamp when
        // the window covers one workspace: the start names no workspace per process.
        let merged: Vec<MergedStderr> = match (&registered, self.roots.as_slice()) {
            (Ok((files, processes)), [_]) => processes
                .iter()
                .filter(|process| process.label.starts_with(PROXY_LABEL))
                .map(|process| MergedStderr {
                    pid: process.pid,
                    tail: file_tail(&files.stderr(process.pid), ""),
                })
                .collect(),
            _ => Vec::new(),
        };
        let mut workspaces: Vec<WorkspaceReads> = self
            .roots
            .iter()
            .map(|root| WorkspaceReads::records(root, &since, &until, &budget))
            .collect();
        for workspace in &mut workspaces {
            workspace.scan_in_flight(&since, &until, &budget);
        }
        for workspace in &workspaces {
            text.push_str(&workspace.text(&merged));
        }
        text.push_str(&processes_text(registered, &merged));
        text.push_str("==== end of failure window ====\n");
        text
    }

    /// The window's files and the processes its start registers, or the line saying why
    /// the window holds none.
    fn registered(&self) -> Result<(&WindowFiles, Vec<RegisteredProcess>), String> {
        let files = match &self.files {
            Ok(Some(files)) => files,
            Ok(None) => {
                return Err("none recorded: outside nextest the window keeps no files\n".into());
            }
            Err(_) => return Err("none recorded: the start was not written\n".into()),
        };
        match WindowStart::read(&files.start()) {
            Ok(start) => Ok((files, start.processes)),
            Err(error) => Err(format!("not read: {error}\n")),
        }
    }
}

/// Label prefix of a registered `rift mcp` child ([`relayed_proxy_client`]).
const PROXY_LABEL: &str = "rift mcp";

/// The stderr tail of one registered `rift mcp` child the window merges with the store
/// records of its one workspace.
struct MergedStderr {
    pid: u32,
    tail: String,
}

impl MergedStderr {
    /// What opens each of its lines in the merged listing.
    fn prefix(&self) -> String {
        format!("{PROXY_LABEL} {} | ", self.pid)
    }
}

/// The part of the window each registered process holds: its label, its exit status, and
/// the tail of its stderr copy, or where that tail merged.
fn processes_text(
    registered: Result<(&WindowFiles, Vec<RegisteredProcess>), String>,
    merged: &[MergedStderr],
) -> String {
    let mut text = String::from("---- processes the test registered ----\n");
    let (files, processes) = match registered {
        Ok(registered) => registered,
        Err(line) => {
            text.push_str(&line);
            return text;
        }
    };
    if processes.is_empty() {
        text.push_str("none registered\n");
    }
    for process in &processes {
        let stderr = files.stderr(process.pid);
        let tail = if merged.iter().any(|copy| copy.pid == process.pid) {
            format!(
                "merged by timestamp into the workspace's records above, each line opening \
                 with `{PROXY_LABEL} {} | `\n",
                process.pid
            )
        } else {
            stderr_tail(
                &stderr,
                "absent: the harness drained no stderr of this process, or could not create \
                 the copy and said why on the test's stderr",
            )
        };
        let _ = writeln!(
            text,
            "---- process {pid}: {label} ----\nexit: {exit}\n---- {path} ----\n{tail}",
            pid = process.pid,
            label = process.label,
            exit = process.exit.as_deref().unwrap_or(EXIT_NOT_OBSERVED),
            path = stderr.display(),
        );
    }
    text
}

/// The host facts a window prints once, in the keys of `rift_dev.machine.machine_line`.
/// The harness carries no reader of physical memory, so `memory_bytes` says so.
fn machine_line() -> String {
    let logical_cpus = std::thread::available_parallelism().map_or_else(
        |error| format!("not read: {error}"),
        |count| count.to_string(),
    );
    format!(
        "machine: logical_cpus={logical_cpus} memory_bytes=not read by the harness system={} \
         architecture={}\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

impl Drop for FailureWindow {
    fn drop(&mut self) {
        let files = self.files.as_ref().ok().and_then(Option::as_ref);
        if !self.open {
            if let Some(files) = files {
                files.remove_all();
            }
            return;
        }
        let ended_at = jiff::Timestamp::now();
        let text = self.text(ended_at);
        let _ = std::io::stderr().write_all(text.as_bytes());
        let Some(files) = files else {
            return;
        };
        if self.began.is_none() {
            // A restored window is printed once, by `print_ended_windows`.
            files.remove_all();
            return;
        }
        // The text lands first and `ended_at` last, so a start carrying `ended_at` has its
        // text beside it, and a kill during the reads leaves a window `print_ended_windows`
        // prints.
        let kept = fs::write(files.text(), &text)
            .and_then(|()| files.append("ended_at", &ended_at.to_string()));
        if let Err(error) = kept {
            let _ = writeln!(
                std::io::stderr(),
                "[the window's files in {} were not completed: {error}]",
                files.directory.display()
            );
        }
    }
}

/// The files of one test attempt's failure window in the report directory of the running
/// nextest profile, each named after the attempt id ([`file_name_of`]):
///
/// - `<stem>.window`, the start: one `key=value` line each, `test`, `attempt`,
///   `started_at`, and `root` per workspace when the window begins; `process=<pid> <label>`
///   per registered process; `exit=<pid> <exit status>` per observed exit; `ended_at`, in
///   RFC 3339, when the window failed in the test's own process.
/// - `<stem>.text`, what that window printed.
/// - `<stem>.<pid>.stderr`, the copy of one registered process's stderr.
#[derive(Clone, Debug)]
pub(crate) struct WindowFiles {
    directory: PathBuf,
    stem: String,
}

impl WindowFiles {
    /// The files of the running nextest attempt; nothing outside nextest.
    fn of_this_test() -> Option<Self> {
        let (Ok(workspace), Ok(profile), Ok(attempt)) = (
            std::env::var("NEXTEST_WORKSPACE_ROOT"),
            std::env::var("NEXTEST_PROFILE"),
            std::env::var("NEXTEST_ATTEMPT_ID"),
        ) else {
            return None;
        };
        let directory = Path::new(&workspace)
            .join(REPORT_DIRECTORY)
            .join(profile)
            .join(OPEN_WINDOWS_DIRECTORY);
        Some(Self::new(directory, &attempt))
    }

    /// The files of the attempt `attempt` in `directory`.
    pub(crate) fn new(directory: PathBuf, attempt: &str) -> Self {
        Self {
            directory,
            stem: file_name_of(attempt),
        }
    }

    /// The files whose start is `path`.
    #[allow(
        dead_code,
        reason = "`server_cli` alone prints ended windows; each suite compiles this file"
    )]
    fn of_start(path: &Path) -> TestResult<Self> {
        let directory = path
            .parent()
            .ok_or_else(|| format!("{}: no directory", path.display()))?;
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| format!("{}: no UTF-8 stem", path.display()))?;
        Ok(Self {
            directory: directory.to_owned(),
            stem: stem.to_owned(),
        })
    }

    pub(crate) fn start(&self) -> PathBuf {
        self.directory
            .join(format!("{}.{START_FILE_EXTENSION}", self.stem))
    }

    pub(crate) fn text(&self) -> PathBuf {
        self.directory
            .join(format!("{}.{TEXT_FILE_EXTENSION}", self.stem))
    }

    pub(crate) fn stderr(&self, pid: u32) -> PathBuf {
        self.directory
            .join(format!("{}.{pid}.{STDERR_FILE_EXTENSION}", self.stem))
    }

    /// Appends the line `key=value` to the start; a test whose window has no start
    /// writes nothing.
    fn append(&self, key: &str, value: &str) -> std::io::Result<()> {
        match fs::OpenOptions::new().append(true).open(self.start()) {
            Ok(mut start) => start.write_all(format!("{key}={value}\n").as_bytes()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Records the process `pid` the test spawned, under `label`.
    pub(crate) fn register_process(&self, pid: u32, label: &str) -> std::io::Result<()> {
        self.append("process", &format!("{pid} {}", label.replace('\n', " ")))
    }

    /// Records the exit the test observed of the registered process `pid`, once per
    /// process; the exit of a process the start does not register is not recorded.
    pub(crate) fn record_exit(
        &self,
        pid: u32,
        status: std::process::ExitStatus,
    ) -> std::io::Result<()> {
        let start = match fs::read_to_string(self.start()) {
            Ok(start) => start,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let registered = format!("process={pid} ");
        let recorded = format!("exit={pid} ");
        if !start.lines().any(|line| line.starts_with(&registered))
            || start.lines().any(|line| line.starts_with(&recorded))
        {
            return Ok(());
        }
        self.append("exit", &format!("{pid} {status:?}"))
    }

    /// A new copy of the stderr of the process `pid`, when the window has a start.
    pub(crate) fn stderr_copy(&self, pid: u32) -> std::io::Result<Option<StderrCopy>> {
        if !self.start().exists() {
            return Ok(None);
        }
        Ok(Some(StderrCopy {
            file: fs::File::create(self.stderr(pid))?,
            written: 0,
            discarded: 0,
            cut: false,
        }))
    }

    /// Removes the start, the text, and every stderr copy, best effort.
    fn remove_all(&self) {
        let _ = fs::remove_file(self.start());
        let _ = fs::remove_file(self.text());
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return;
        };
        let prefix = format!("{}.", self.stem);
        let suffix = format!(".{STDERR_FILE_EXTENSION}");
        for entry in entries.flatten() {
            let name = entry.file_name();
            let copy = name
                .to_str()
                .and_then(|name| name.strip_prefix(&prefix))
                .and_then(|rest| rest.strip_suffix(&suffix))
                .is_some_and(|pid| {
                    !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit())
                });
            if copy {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// Records, in the running test's failure window, the process `pid` it spawned under
/// `label`, such as `rift server start --foreground`; a test with no open window under
/// nextest records nothing.
pub(crate) fn register_process(pid: u32, label: &str) {
    if let Some(files) = WindowFiles::of_this_test()
        && let Err(error) = files.register_process(pid, label)
    {
        let _ = writeln!(
            std::io::stderr(),
            "[the window did not record process {pid} ({label}): {error}]"
        );
    }
}

/// Records, in the running test's failure window, the exit status the test observed of
/// the process `pid`; only its first observation is recorded.
pub(crate) fn record_exit(pid: u32, status: std::process::ExitStatus) {
    if let Some(files) = WindowFiles::of_this_test()
        && let Err(error) = files.record_exit(pid, status)
    {
        let _ = writeln!(
            std::io::stderr(),
            "[the window did not record the exit of process {pid}: {error}]"
        );
    }
}

/// A new copy of the stderr of the process `pid` in the running test's failure window,
/// when the window has a start; why it could not be created goes to the test's stderr.
pub(crate) fn stderr_copy(pid: u32) -> Option<StderrCopy> {
    let files = WindowFiles::of_this_test()?;
    files.stderr_copy(pid).unwrap_or_else(|error| {
        let _ = writeln!(
            std::io::stderr(),
            "[the window keeps no stderr copy of process {pid}: {}: {error}]",
            files.stderr(pid).display()
        );
        None
    })
}

/// The copy of one registered process's stderr, bounded by [`STDERR_FILE_BYTES_MAX`].
pub(crate) struct StderrCopy {
    file: fs::File,
    written: u64,
    /// Bytes read past the bound and dropped.
    discarded: u64,
    cut: bool,
}

impl StderrCopy {
    /// Appends `bytes`, up to the bound; the first write the bound cuts appends one
    /// notice line, and every later write is dropped and counted in `discarded`.
    pub(crate) fn write(&mut self, bytes: &[u8]) {
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if self.cut {
            self.discarded = self.discarded.saturating_add(length);
            return;
        }
        let room = STDERR_FILE_BYTES_MAX.saturating_sub(self.written);
        let kept = bytes.len().min(usize::try_from(room).unwrap_or(usize::MAX));
        let _ = self.file.write_all(&bytes[..kept]);
        let kept_length = u64::try_from(kept).unwrap_or(u64::MAX);
        self.written = self.written.saturating_add(kept_length);
        self.discarded = self
            .discarded
            .saturating_add(length.saturating_sub(kept_length));
        if kept < bytes.len() {
            self.cut = true;
            let _ = writeln!(
                self.file,
                "\n[the copy reached its {STDERR_FILE_BYTES_MAX}-byte bound; the rest is read \
                 and dropped]"
            );
        }
    }
}

impl Drop for StderrCopy {
    /// Ends a cut copy with the count of bytes its bound dropped, once its stream closed.
    fn drop(&mut self) {
        if self.cut {
            let _ = writeln!(
                self.file,
                "[{} bytes past the {STDERR_FILE_BYTES_MAX}-byte bound were read and dropped]",
                self.discarded
            );
        }
    }
}

/// One registered process of a window's start.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RegisteredProcess {
    pub(crate) pid: u32,
    pub(crate) label: String,
    /// The exit status the test observed, as `Debug` printed it.
    pub(crate) exit: Option<String>,
}

/// What one start file holds.
#[derive(Debug)]
pub(crate) struct WindowStart {
    pub(crate) test: String,
    #[allow(
        dead_code,
        reason = "`server_cli` alone reads it back; each suite compiles this file"
    )]
    pub(crate) attempt: Option<String>,
    pub(crate) started_at: jiff::Timestamp,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) processes: Vec<RegisteredProcess>,
    /// When the window failed in the test's own process.
    #[allow(
        dead_code,
        reason = "`server_cli` alone reads it back; each suite compiles this file"
    )]
    pub(crate) ended_at: Option<jiff::Timestamp>,
}

impl WindowStart {
    /// Reads the start file at `path`; an exit of a process the start does not register
    /// is refused.
    pub(crate) fn read(path: &Path) -> TestResult<Self> {
        let refused = |what: String| format!("{}: {what}", path.display());
        let contents = fs::read_to_string(path)?;
        let mut test = None;
        let mut attempt = None;
        let mut started_at = None;
        let mut ended_at = None;
        let mut roots = Vec::new();
        let mut processes: Vec<RegisteredProcess> = Vec::new();
        for line in contents.lines() {
            let pid_and = |value: &str| -> TestResult<(u32, String)> {
                let (pid, rest) = value
                    .split_once(' ')
                    .ok_or_else(|| refused(format!("no pid and text in {line:?}")))?;
                Ok((pid.parse()?, rest.to_owned()))
            };
            match line.split_once('=') {
                Some(("test", value)) => test = Some(value.to_owned()),
                Some(("attempt", value)) => attempt = Some(value.to_owned()),
                Some(("started_at", value)) => started_at = Some(value.parse()?),
                Some(("ended_at", value)) => ended_at = Some(value.parse()?),
                Some(("root", value)) => roots.push(PathBuf::from(value)),
                Some(("process", value)) => {
                    let (pid, label) = pid_and(value)?;
                    processes.push(RegisteredProcess {
                        pid,
                        label,
                        exit: None,
                    });
                }
                Some(("exit", value)) => {
                    let (pid, status) = pid_and(value)?;
                    let process = processes
                        .iter_mut()
                        .find(|process| process.pid == pid)
                        .ok_or_else(|| refused(format!("exit of unregistered process {pid}")))?;
                    process.exit = Some(status);
                }
                _ => return Err(refused(format!("unexpected line {line:?}")).into()),
            }
        }
        Ok(Self {
            test: test.ok_or_else(|| refused("no test line".to_owned()))?,
            attempt,
            started_at: started_at.ok_or_else(|| refused("no started_at line".to_owned()))?,
            roots,
            processes,
            ended_at,
        })
    }
}

/// The test's name and the nextest variables that identify its run and attempt.
fn test_identity() -> String {
    let test = std::thread::current()
        .name()
        .unwrap_or("unnamed test")
        .to_owned();
    let identity: Vec<String> = [
        "NEXTEST_BINARY_ID",
        "NEXTEST_TEST_NAME",
        "NEXTEST_ATTEMPT_ID",
    ]
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
/// start files without `ended_at` left in the report directory of every profile, and
/// removes the files of each window it printed. Answers how many it printed. At most
/// [`ENDED_WINDOWS_MAX`] print; the rest are named, and kept for the next run. The files
/// of a window that failed in its test's own process carry `ended_at`; it leaves them.
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
    let starts = killed_window_starts(&Path::new(&workspace).join(REPORT_DIRECTORY))?;
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

/// The start files, sorted, below every profile directory of `reports` whose window
/// carries no `ended_at`: the test's process ended before its window printed.
#[allow(
    dead_code,
    reason = "`server_cli` alone prints ended windows; each suite compiles this file"
)]
pub(crate) fn killed_window_starts(reports: &Path) -> TestResult<Vec<PathBuf>> {
    let mut starts = Vec::new();
    if let Ok(profiles) = fs::read_dir(reports) {
        for profile in profiles {
            let directory = profile?.path().join(OPEN_WINDOWS_DIRECTORY);
            let Ok(files) = fs::read_dir(&directory) else {
                continue;
            };
            for file in files {
                let path = file?.path();
                if path.extension() == Some(START_FILE_EXTENSION.as_ref())
                    && !fs::read_to_string(&path)?
                        .lines()
                        .any(|line| line.starts_with("ended_at="))
                {
                    starts.push(path);
                }
            }
        }
    }
    starts.sort();
    Ok(starts)
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
    /// The command line that read the log records of the window.
    heading: String,
    /// What the read printed, bounded by [`WINDOW_SOURCE_BYTES_MAX`].
    printed: String,
    /// How long the read ran, what its bound cut, or why it did not finish.
    notes: String,
    in_flight: InFlight,
}

impl WorkspaceReads {
    /// Reads the newest [`WINDOW_RECORDS_MAX`] log records of the window in `root`.
    fn records(root: &Path, since: &str, until: &str, budget: &ReadBudget) -> Self {
        let tail = WINDOW_RECORDS_MAX.to_string();
        let arguments = window_arguments(since, until, &tail);
        let heading = read_heading(&arguments);
        let mut printed = String::new();
        let mut notes = String::new();
        let in_flight = match read_window(root, &arguments, budget) {
            Ok(read) => {
                let count = read.printed.lines().filter(|line| !line.is_empty()).count();
                let newest = newest_in_flight(read.printed.lines());
                printed = bounded_tail(&read.printed);
                notes.push_str(&read.took);
                if count >= WINDOW_RECORDS_MAX {
                    let _ = writeln!(
                        notes,
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
                let _ = writeln!(
                    notes,
                    "the record read did not finish: {error}\n[the window falls back to the \
                     stderr files below]"
                );
                InFlight::NotRead(format!("the record read did not finish: {error}"))
            }
        };
        Self {
            root: root.to_owned(),
            heading,
            printed,
            notes,
            in_flight,
        }
    }

    /// Scans every log record of the window for the newest record of the table of
    /// operations in flight, when the bounded read cut older records.
    fn scan_in_flight(&mut self, since: &str, until: &str, budget: &ReadBudget) {
        use std::io::BufRead as _;

        if !matches!(self.in_flight, InFlight::Cut) {
            return;
        }
        let arguments = window_arguments(since, until, "all");
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

    /// The part of a failure window this workspace holds: its records of the window, with
    /// the lines of every `merged` stderr merged in by timestamp, then its server's stderr
    /// file.
    fn text(&self, merged: &[MergedStderr]) -> String {
        let mut text = format!("---- workspace {} ----\n", self.root.display());
        text.push_str(&self.heading);
        if merged.is_empty() {
            text.push_str(&self.printed);
        } else {
            let prefixes: Vec<String> = merged.iter().map(MergedStderr::prefix).collect();
            let _ = writeln!(
                text,
                "[merged by timestamp with the stderr of each line opening with {}]",
                prefixes
                    .iter()
                    .map(|prefix| format!("`{prefix}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let mut sources = vec![("", self.printed.as_str())];
            sources.extend(
                prefixes
                    .iter()
                    .zip(merged)
                    .map(|(prefix, copy)| (prefix.as_str(), copy.tail.as_str())),
            );
            text.push_str(&merged_by_timestamp(&sources));
        }
        text.push_str(&self.notes);
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
        let stderr_file = rift_mcp::stderr_file_path(&self.root);
        let _ = writeln!(
            text,
            "---- {} ----\n{}",
            stderr_file.display(),
            stderr_tail(
                &stderr_file,
                "absent: only a server `rift server start` spawned writes this file"
            )
        );
        text
    }
}

/// What opens the line a server's log drain writes to stderr when the log store refuses
/// a batch (`write_retained`, `crates/rift-tracing/src/drain.rs`).
const STORE_REFUSAL: &str = "rift: the log store refused a batch";

/// [`file_tail`] of a stderr file, led by a line saying the window falls back to it when
/// the file holds [`STORE_REFUSAL`]: the store lacks the records the drain kept retrying.
fn stderr_tail(path: &Path, absent: &str) -> String {
    let tail = file_tail(path, absent);
    if !holds_store_refusal(path) {
        return tail;
    }
    format!(
        "[the log store refused writes (`{STORE_REFUSAL}`): the window falls back to this \
         stderr file for the records the store lacks]\n{tail}"
    )
}

/// Whether the file at `path`, read up to [`STDERR_FILE_BYTES_MAX`] bytes, holds a line
/// opening with [`STORE_REFUSAL`].
fn holds_store_refusal(path: &Path) -> bool {
    use std::io::BufRead as _;

    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    std::io::BufReader::new(file.take(STDERR_FILE_BYTES_MAX))
        .split(b'\n')
        .map_while(Result::ok)
        .any(|line| line.starts_with(STORE_REFUSAL.as_bytes()))
}

/// Characters of the timestamp a printed line opens with, `2026-10-04 20:42:58.787Z`.
const PRINTED_TIMESTAMP_CHARS: usize = 24;

/// The timestamp `line` opens with when it is a UTC time with milliseconds,
/// `YYYY-MM-DD HH:MM:SS.mmmZ`, the form every surface prints.
pub(crate) fn printed_timestamp(line: &str) -> Option<&str> {
    let stamp = line.get(..PRINTED_TIMESTAMP_CHARS)?;
    let bytes = stamp.as_bytes();
    let shaped = bytes[10] == b' '
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && stamp.chars().filter(char::is_ascii_digit).count() == 17;
    shaped.then_some(stamp)
}

/// The lines of every `(prefix, text)` source merged by the timestamp each line opens
/// with, `prefix` opening each line of its source.
///
/// A line that opens with no timestamp stays after the line before it in its source;
/// the lines ahead of a source's first timestamp come first. Lines of one timestamp keep
/// the order of `sources`, and blank lines are left out: a group spans one source.
pub(crate) fn merged_by_timestamp(sources: &[(&str, &str)]) -> String {
    // (timestamp, the source's lines from it to the next timestamp)
    let mut entries: Vec<(Option<&str>, Vec<String>)> = Vec::new();
    for (prefix, text) in sources {
        let mut entry: Option<(Option<&str>, Vec<String>)> = None;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let stamp = printed_timestamp(line);
            if stamp.is_some() || entry.is_none() {
                entries.extend(entry.take());
                entry = Some((stamp, Vec::new()));
            }
            if let Some((_, lines)) = &mut entry {
                lines.push(format!("{prefix}{line}"));
            }
        }
        entries.extend(entry);
    }
    // A stable sort: one timestamp keeps the order of the sources.
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut merged = String::new();
    for line in entries.iter().flat_map(|(_, lines)| lines) {
        merged.push_str(line);
        merged.push('\n');
    }
    merged
}

/// The window query arguments of one read: `--since`, `--until`, `--tail`.
fn window_arguments<'a>(since: &'a str, until: &'a str, tail: &'a str) -> [&'a str; 6] {
    ["--since", since, "--until", until, "--tail", tail]
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

/// The last [`WINDOW_SOURCE_BYTES_MAX`] bytes of the file at `path`, `absent` when there
/// is no such file, or why it could not be read.
fn file_tail(path: &Path, absent: &str) -> String {
    match fs::File::open(path) {
        Ok(mut file) => tail_of(&mut file).unwrap_or_else(|error| format!("unreadable: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => absent.to_owned(),
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
    // The command is built on the test's thread, whose name outside nextest is the
    // test case name the child carries.
    let mut command = std::process::Command::new(rift_binary());
    with_child_log_variables(&mut command)
        .args(arguments)
        .current_dir(root)
        .stdin(Stdio::null());
    let output = tokio::task::spawn_blocking(move || command.output()).await??;
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
/// spawns inherits them. `arguments` follow the `mcp` subcommand.
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
    let copy = transport.id().and_then(|pid| {
        register_process(
            pid,
            &["rift", "mcp"]
                .iter()
                .chain(arguments)
                .copied()
                .collect::<Vec<_>>()
                .join(" "),
        );
        stderr_copy(pid)
    });
    let stderr = RelayedStderr::spawn(reader, copy);
    Ok((().serve(transport).await?, stderr))
}

/// Takes the piped stderr of `child`, a `rift` process the test spawned, registers the
/// child in the test's failure window under `label`, and relays the stream like a
/// `rift mcp` child's.
pub(crate) fn relayed_child_stderr(
    child: &mut std::process::Child,
    label: &str,
) -> TestResult<RelayedStderr> {
    let stream = child.stderr.take().ok_or("the child's stderr is piped")?;
    register_process(child.id(), label);
    Ok(RelayedStderr::spawn(stream, stderr_copy(child.id())))
}

/// Bytes of one child's stderr the relay writes and keeps; the rest is read
/// and dropped, so the child never blocks on a full pipe.
const RELAYED_STDERR_BYTES_MAX: usize = 1 << 20;

/// A child's stderr, copied onto this test process's stderr as it arrives,
/// into the child's [`StderrCopy`] when the test's window keeps one, and kept
/// for the test to assert on.
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
    pub(crate) fn spawn(stream: impl Read + Send + 'static, copy: Option<StderrCopy>) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&bytes);
        Self {
            bytes,
            relay: std::thread::spawn(move || relay_until_closed(stream, &captured, copy)),
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
/// `copy` receives every byte read, under its own bound.
fn relay_until_closed(
    mut stream: impl Read,
    captured: &Mutex<Vec<u8>>,
    mut copy: Option<StderrCopy>,
) {
    let mut buffer = [0_u8; rift_core::STREAM_READ_BYTES];
    let mut cut_reported = false;
    loop {
        let read_bytes = match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read_bytes) => read_bytes,
        };
        if let Some(copy) = &mut copy {
            copy.write(&buffer[..read_bytes]);
        }
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
