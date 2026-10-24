//! Query children own parsing, optimization, evaluation and result iteration.

use std::io::{BufWriter, Read, Write};
use std::path::Path;
use std::time::Instant;

use rift_error::{RiftError, errors};
use rift_protocol::query::QueryAnswer;
use rift_search::sparql::{
    PreparedQuery, QueryCancellation, QueryFacts, QueryLimits, QueryPlan, QueryServices,
};
use serde::{Deserialize, Serialize};

use crate::process::{BoundedRun, Command, RunEnding, run_bounded};

/// Hidden command shared by the runner and the installed Rift executable.
pub const QUERY_CHILD_COMMAND: &str = "__query";
/// Space for metadata, JSON framing and the child executable's initial allocations.
const PROCESS_OVERHEAD_BYTES: usize = 32 * 1024 * 1024;
/// Diagnostic prefix retained from either child stream; query data uses owned files.
const DIAGNOSTIC_BYTES_MAX: usize = 8 * 1024;

/// Finite byte ceilings derived from the accepted statement, fact and result bounds.
#[derive(Clone, Copy, Debug)]
pub struct QueryProcessLimits {
    /// Most serialized bytes exchanged with a query child.
    pub bytes_max: usize,
    /// Most live Rust allocation bytes in a query child.
    pub heap_bytes_max: usize,
}

impl QueryProcessLimits {
    /// Derives child limits without changing the accepted RDF or answer limits.
    ///
    /// The exchange allows four copies of the accepted data sizes for JSON encoding
    /// and metadata. The heap allows two copies of the accepted data sizes plus initial
    /// process allocations.
    /// Crossing either ceiling refuses the entire query.
    ///
    /// # Errors
    /// Returns unavailable when the accepted sizes exceed platform arithmetic.
    pub fn from_query(limits: QueryLimits) -> Result<Self, RiftError> {
        let sum = limits
            .query_bytes_max
            .checked_add(limits.rdf_bytes_max)
            .and_then(|bytes| bytes.checked_add(limits.result_bytes_max));
        let heap_bytes_max = sum
            .and_then(|bytes| bytes.checked_mul(2))
            .and_then(|bytes| bytes.checked_add(PROCESS_OVERHEAD_BYTES))
            .ok_or_else(|| unavailable("query process heap limit exceeds the platform bound"))?;
        let bytes_max = sum
            .and_then(|bytes| bytes.checked_mul(4))
            .and_then(|bytes| bytes.checked_add(PROCESS_OVERHEAD_BYTES))
            .ok_or_else(|| unavailable("query process byte limits exceed the platform bound"))?;
        Ok(Self {
            bytes_max,
            heap_bytes_max,
        })
    }
}

/// Additional admission for owned query records before exchange serialization.
///
/// This counts each complete record with the exchange codec before collection.
/// The evaluator separately enforces the accepted projected RDF term byte bound.
pub struct QueryFactBudget<'a> {
    writer: BoundedWriter<std::io::Sink>,
    cancelled: &'a (dyn Fn() -> bool + Sync),
}

impl<'a> QueryFactBudget<'a> {
    /// Creates record admission from the same finite ceiling as the child exchange.
    ///
    /// # Errors
    /// Returns unavailable when accepted sizes exceed platform arithmetic.
    pub fn new(
        limits: QueryLimits,
        cancelled: &'a (dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        Ok(Self {
            writer: BoundedWriter {
                writer: std::io::sink(),
                remaining: QueryProcessLimits::from_query(limits)?.bytes_max,
            },
            cancelled,
        })
    }

    /// Admits one borrowed record before its owned copy joins the query snapshot.
    ///
    /// # Errors
    /// Returns cancellation, encoding failure or exhausted record admission.
    pub fn charge(&mut self, value: &impl Serialize) -> Result<(), RiftError> {
        self.check()?;
        let active = || !(self.cancelled)();
        let result = serde_json::to_writer(
            RequestWriter {
                writer: &mut self.writer,
                active: &active,
            },
            value,
        );
        self.check()?;
        result.map_err(process_error)?;
        self.writer.write_all(b",").map_err(process_error)
    }

    /// Admits already encoded material before decoding an owned record.
    ///
    /// # Errors
    /// Returns cancellation or exhausted record admission.
    pub fn charge_bytes(&mut self, bytes: &[u8]) -> Result<(), RiftError> {
        self.check()?;
        self.writer.write_all(bytes).map_err(process_error)
    }

    /// Checks cancellation between records, including records excluded by selection.
    ///
    /// # Errors
    /// Returns the registered cancellation failure.
    pub fn check(&self) -> Result<(), RiftError> {
        if (self.cancelled)() {
            return Err(errors::server::read_cancelled().error());
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
enum QueryTask {
    Prepare {
        statement: String,
        limits: QueryLimits,
    },
    Evaluate {
        plan: Box<QueryPlan>,
        facts: Box<QueryFacts>,
        limits: QueryLimits,
        services: Box<QueryServices>,
    },
}

#[derive(Deserialize, Serialize)]
enum QueryOutput {
    Plan(Box<QueryPlan>),
    Answer(QueryAnswer),
}

#[derive(Deserialize, Serialize)]
enum QueryFailure {
    Invalid { violation: String },
    Unavailable { operation: String, detail: String },
    Cancelled,
}

impl QueryFailure {
    fn capture(error: RiftError) -> Self {
        let field = |name| {
            error
                .context()
                .find_map(|(key, value)| (key == name).then_some(value.to_owned()))
        };
        match error.slug().as_str() {
            "rift.server.read_invalid" => Self::Invalid {
                violation: field("violation").unwrap_or_else(|| error.to_string()),
            },
            "rift.server.read_cancelled" => Self::Cancelled,
            _ => Self::Unavailable {
                operation: field("operation").unwrap_or_else(|| "query child".to_owned()),
                detail: field("detail").unwrap_or_else(|| error.to_string()),
            },
        }
    }

    fn into_error(self) -> RiftError {
        match self {
            Self::Invalid { violation } => errors::server::read_invalid()
                .field("query")
                .violation(violation)
                .error(),
            Self::Unavailable { operation, detail } => errors::server::read_unavailable()
                .operation(operation)
                .detail(detail)
                .error(),
            Self::Cancelled => errors::server::read_cancelled().error(),
        }
    }
}

type QueryResponse = Result<QueryOutput, QueryFailure>;

/// Parses and plans one statement in an owned child under the caller's deadline.
///
/// # Errors
/// Returns parser, admission, cancellation, process or deadline failures.
pub fn prepare_query(
    program: &Path,
    statement: String,
    limits: QueryLimits,
    deadline: Instant,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<QueryPlan, RiftError> {
    match execute(
        program,
        QueryTask::Prepare { statement, limits },
        limits,
        deadline,
        cancelled,
    )? {
        QueryOutput::Plan(plan) => Ok(*plan),
        QueryOutput::Answer(_) => Err(unavailable("query child returned an unexpected answer")),
    }
}

/// Evaluates a query and collects its complete bounded result in an owned child.
///
/// # Errors
/// Returns mapping, admission, cancellation, process or evaluation failures.
pub fn evaluate_query(
    program: &Path,
    plan: QueryPlan,
    facts: QueryFacts,
    limits: QueryLimits,
    services: QueryServices,
    deadline: Instant,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<QueryAnswer, RiftError> {
    let task = QueryTask::Evaluate {
        plan: Box::new(plan),
        facts: Box::new(facts),
        limits,
        services: Box::new(services),
    };
    match execute(program, task, limits, deadline, cancelled)? {
        QueryOutput::Answer(answer) => Ok(answer),
        QueryOutput::Plan(_) => Err(unavailable("query child returned an unexpected plan")),
    }
}

fn execute(
    program: &Path,
    task: QueryTask,
    limits: QueryLimits,
    deadline: Instant,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<QueryOutput, RiftError> {
    check_request(deadline, cancelled)?;
    let process_limits = QueryProcessLimits::from_query(limits)?;
    let directory = exchange_directory().map_err(process_error)?;
    let mut request = tempfile::NamedTempFile::new_in(directory.path()).map_err(process_error)?;
    let response = tempfile::NamedTempFile::new_in(directory.path()).map_err(process_error)?;
    let active = || !cancelled() && Instant::now() < deadline;
    let written = write_json(
        RequestWriter {
            writer: &mut request,
            active: &active,
        },
        &task,
        process_limits.bytes_max,
    );
    check_request(deadline, cancelled)?;
    written?;
    drop(task);
    let mut command = Command::new(program);
    command
        .arg(QUERY_CHILD_COMMAND)
        .arg(request.path())
        .arg(response.path())
        .arg(process_limits.bytes_max.to_string())
        .arg(process_limits.heap_bytes_max.to_string());
    let timeout = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| unavailable("query exceeded the request deadline"))?;
    let run = run_bounded(&mut command, timeout, DIAGNOSTIC_BYTES_MAX, cancelled)
        .map_err(process_error)?;
    collect_response(
        run,
        &response,
        process_limits.bytes_max,
        deadline,
        cancelled,
    )
}

fn collect_response(
    run: BoundedRun,
    response: &tempfile::NamedTempFile,
    bytes_max: usize,
    deadline: Instant,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<QueryOutput, RiftError> {
    match run.ending {
        RunEnding::Cancelled => return Err(errors::server::read_cancelled().error()),
        RunEnding::TimedOut => return Err(unavailable("query exceeded the request deadline")),
        RunEnding::Exited => {}
    }
    let exit = run.exit.map_err(process_error)?;
    if !exit.success() {
        return Err(unavailable(format!(
            "query child exited unsuccessfully: {exit}"
        )));
    }
    let bytes = read_request(response.reopen().map_err(process_error)?, bytes_max)?;
    check_request(deadline, cancelled)?;
    let response: QueryResponse = serde_json::from_slice(&bytes).map_err(process_error)?;
    check_request(deadline, cancelled)?;
    response.map_err(QueryFailure::into_error)
}

fn check_request(
    deadline: Instant,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<(), RiftError> {
    if cancelled() {
        return Err(errors::server::read_cancelled().error());
    }
    if Instant::now() >= deadline {
        return Err(unavailable("query exceeded the request deadline"));
    }
    Ok(())
}

fn exchange_directory() -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.tempdir()
}

/// Runs one typed request after the executable enables the child's heap ceiling.
///
/// The caller runs this synchronously before creating runtime or tracing threads.
/// Parsing, optimization, result iteration and serialization all finish before return.
///
/// # Errors
/// Returns file, input or output failures; query failures remain typed in the response.
pub fn run_query_child(
    request_path: &Path,
    response_path: &Path,
    bytes_max: usize,
) -> Result<(), RiftError> {
    let request = std::fs::File::open(request_path).map_err(process_error)?;
    let bytes = read_request(request, bytes_max)?;
    let task: QueryTask = serde_json::from_slice(&bytes).map_err(process_error)?;
    drop(bytes);
    let response = run_task(task).map_err(QueryFailure::capture);
    let output = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(response_path)
        .map_err(process_error)?;
    write_json(output, &response, bytes_max)
}

fn run_task(task: QueryTask) -> Result<QueryOutput, RiftError> {
    match task {
        QueryTask::Prepare { statement, limits } => {
            QueryPlan::prepare(statement, limits).map(|plan| QueryOutput::Plan(Box::new(plan)))
        }
        QueryTask::Evaluate {
            plan,
            facts,
            limits,
            services,
        } => PreparedQuery::parse(plan.statement(), limits)?
            .evaluate_with_services(*facts, limits, QueryCancellation::default(), *services)
            .map(QueryOutput::Answer),
    }
}

fn read_request(reader: impl Read, bytes_max: usize) -> Result<Vec<u8>, RiftError> {
    let ceiling = u64::try_from(bytes_max)
        .ok()
        .and_then(|bytes| bytes.checked_add(1))
        .ok_or_else(|| unavailable("query request byte limit exceeds the platform bound"))?;
    let mut bytes = Vec::new();
    reader
        .take(ceiling)
        .read_to_end(&mut bytes)
        .map_err(process_error)?;
    if bytes.len() > bytes_max {
        return Err(unavailable("query request exceeds its byte limit"));
    }
    Ok(bytes)
}

struct BoundedWriter<W> {
    writer: W,
    remaining: usize,
}

struct RequestWriter<'a, W> {
    writer: W,
    active: &'a dyn Fn() -> bool,
}

impl<W: Write> Write for RequestWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !(self.active)() {
            return Err(std::io::Error::other("query request is no longer active"));
        }
        self.writer.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other(
                "query exchange exceeds its byte limit",
            ));
        }
        let written = self.writer.write(bytes)?;
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

fn write_json(
    writer: impl Write,
    value: &impl Serialize,
    bytes_max: usize,
) -> Result<(), RiftError> {
    let mut bounded = BoundedWriter {
        writer: BufWriter::new(writer),
        remaining: bytes_max,
    };
    serde_json::to_writer(&mut bounded, value).map_err(process_error)?;
    bounded.flush().map_err(process_error)
}

fn process_error(error: impl std::fmt::Display) -> RiftError {
    unavailable(error.to_string())
}

fn unavailable(detail: impl Into<String>) -> RiftError {
    errors::server::read_unavailable()
        .operation("query child")
        .detail(detail.into())
        .error()
}

#[cfg(test)]
mod tests;
