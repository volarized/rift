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

use crate::process::{BoundedRun, Command, RunEnding, run_bounded_streams};

/// Hidden command shared by the runner and the installed Rift executable.
pub const QUERY_CHILD_COMMAND: &str = "__query";
/// Space for metadata, JSON framing and the child executable's initial allocations.
const PROCESS_OVERHEAD_BYTES: usize = 32 * 1024 * 1024;
/// Default diagnostic prefix; the accepted configuration can override it.
const DIAGNOSTIC_BYTES_DEFAULT: usize = 8 * 1024;
// The only stdout frame is {"version":<u32>,"payload":null}. Its longest version
// is u32::MAX; every other byte is fixed by the strict exchange structure.
const ACKNOWLEDGMENT_BYTES_MAX: usize = b"{\"version\":4294967295,\"payload\":null}".len();
// Change the version when the query snapshot contract changes. Zero is not a supported version.
const QUERY_EXCHANGE_VERSION: u32 = 8;

/// Versioned query data shared by the child and authoritative fact reads.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryExchange<T> {
    version: u32,
    payload: T,
}

impl<T> QueryExchange<T> {
    /// Carries one payload under the current query snapshot contract.
    #[must_use]
    pub fn new(payload: T) -> Self {
        Self {
            version: QUERY_EXCHANGE_VERSION,
            payload,
        }
    }

    /// Accepts the payload only when its query snapshot contract matches.
    ///
    /// # Errors
    /// Returns the registered unsupported failure for an incompatible version.
    pub fn into_payload(self) -> Result<T, RiftError> {
        if self.version != QUERY_EXCHANGE_VERSION {
            return Err(incompatible("query exchange version is incompatible"));
        }
        Ok(self.payload)
    }
}

/// Finite byte ceilings derived from the accepted statement, fact and result bounds.
#[derive(Clone, Copy, Debug)]
pub struct QueryProcessLimits {
    /// Most serialized request bytes sent to a query child.
    pub input_bytes_max: usize,
    /// Most serialized response bytes read from a query child.
    pub output_bytes_max: usize,
    /// Most acquired material bytes retained by one request.
    pub material_bytes_max: usize,
    /// Most live Rust allocation bytes in a query child.
    pub heap_bytes_max: usize,
    /// Most standard error bytes retained from a query child.
    pub diagnostic_bytes_max: usize,
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
        let derived = |copies| {
            limits
                .query_bytes_max
                .checked_add(limits.rdf_bytes_max)
                .and_then(|bytes| bytes.checked_add(limits.result_bytes_max))
                .and_then(|bytes| bytes.checked_mul(copies))
                .and_then(|bytes| bytes.checked_add(PROCESS_OVERHEAD_BYTES))
                .ok_or_else(|| unavailable("query process byte limits exceed the platform bound"))
        };
        let resolve = |explicit: Option<usize>, copies| {
            let bytes = explicit.map_or_else(|| derived(copies), Ok)?;
            if bytes == 0 {
                return Err(unavailable("query process byte limit must be positive"));
            }
            Ok(bytes)
        };
        Ok(Self {
            input_bytes_max: resolve(limits.input_bytes_max, 4)?,
            output_bytes_max: resolve(limits.output_bytes_max, 4)?,
            material_bytes_max: resolve(limits.material_bytes_max, 4)?,
            heap_bytes_max: resolve(limits.heap_bytes_max, 2)?,
            diagnostic_bytes_max: resolve(
                Some(
                    limits
                        .diagnostic_bytes_max
                        .unwrap_or(DIAGNOSTIC_BYTES_DEFAULT),
                ),
                1,
            )?,
        })
    }
}

/// Additional admission for owned query records before exchange serialization.
///
/// Producer handles share one remaining byte count and the first request failure.
/// Records debit encoded bytes or their owner's retained material before collection.
/// The evaluator separately enforces the accepted projected RDF term byte bound.
#[derive(Clone)]
pub struct QueryFactBudget<'a> {
    state: std::sync::Arc<std::sync::Mutex<QueryFactAdmission>>,
    cancelled: &'a (dyn Fn() -> bool + Sync),
    deadline: Option<Instant>,
}

struct QueryFactAdmission {
    writer: BoundedWriter<std::io::Sink>,
    failure: Option<QueryFailure>,
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
            state: std::sync::Arc::new(std::sync::Mutex::new(QueryFactAdmission {
                writer: BoundedWriter {
                    writer: std::io::sink(),
                    remaining: QueryProcessLimits::from_query(limits)?.material_bytes_max,
                },
                failure: None,
            })),
            cancelled,
            deadline: None,
        })
    }

    /// Creates one request admission with an unchanged deadline across producer handles.
    ///
    /// # Errors
    /// Returns unavailable when accepted sizes exceed platform arithmetic.
    pub fn for_request(
        limits: QueryLimits,
        deadline: Instant,
        cancelled: &'a (dyn Fn() -> bool + Sync),
    ) -> Result<Self, RiftError> {
        let mut budget = Self::new(limits, cancelled)?;
        budget.deadline = Some(deadline);
        Ok(budget)
    }

    /// Preserves accumulated admission while changing the active worker's cancellation check.
    #[must_use]
    pub fn with_cancellation<'next>(
        &self,
        cancelled: &'next (dyn Fn() -> bool + Sync),
    ) -> QueryFactBudget<'next> {
        QueryFactBudget {
            state: std::sync::Arc::clone(&self.state),
            cancelled,
            deadline: self.deadline,
        }
    }

    /// Admits one borrowed record before its owned copy joins the query snapshot.
    ///
    /// # Errors
    /// Returns cancellation, encoding failure or exhausted record admission.
    pub fn charge(&mut self, value: &impl Serialize) -> Result<(), RiftError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.check_state(&mut state)?;
        let observed = std::cell::RefCell::new(None);
        let active = || {
            if let Some(failure) = self.observed_failure() {
                observed.borrow_mut().get_or_insert(failure);
                false
            } else {
                true
            }
        };
        let result = serde_json::to_writer(
            RequestWriter {
                writer: &mut state.writer,
                active: &active,
            },
            value,
        );
        if let Err(error) = result {
            let failure = observed
                .into_inner()
                .unwrap_or_else(|| admission_failure(error));
            state.failure = Some(failure.clone());
            return Err(failure.into_error());
        }
        self.check_state(&mut state)?;
        if let Err(error) = state.writer.write_all(b",") {
            let failure = admission_failure(error);
            state.failure = Some(failure.clone());
            return Err(failure.into_error());
        }
        Ok(())
    }

    /// Admits already encoded material before decoding an owned record.
    ///
    /// # Errors
    /// Returns cancellation or exhausted record admission.
    pub fn charge_bytes(&mut self, bytes: &[u8]) -> Result<(), RiftError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.check_state(&mut state)?;
        if let Err(error) = state.writer.write_all(bytes) {
            let failure = admission_failure(error);
            state.failure = Some(failure.clone());
            return Err(failure.into_error());
        }
        Ok(())
    }

    /// Debits a retained material quantity measured by the existing producer owner.
    ///
    /// This quantity is distinct from encoded JSON bytes and projected RDF term bytes.
    ///
    /// # Errors
    /// Returns the first admission failure, cancellation or expired request deadline.
    pub fn charge_material(&mut self, bytes: usize) -> Result<(), RiftError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.check_state(&mut state)?;
        let Some(remaining) = state.writer.remaining.checked_sub(bytes) else {
            let failure = admission_failure("query retained material exceeds its byte limit");
            state.failure = Some(failure.clone());
            return Err(failure.into_error());
        };
        state.writer.remaining = remaining;
        Ok(())
    }

    /// Checks cancellation between records, including records excluded by selection.
    ///
    /// # Errors
    /// Returns the registered cancellation failure.
    pub fn check(&self) -> Result<(), RiftError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.check_state(&mut state)
    }

    fn check_state(&self, state: &mut QueryFactAdmission) -> Result<(), RiftError> {
        if let Some(failure) = &state.failure {
            return Err(failure.clone().into_error());
        }
        if let Some(failure) = self.observed_failure() {
            state.failure = Some(failure.clone());
            return Err(failure.into_error());
        }
        Ok(())
    }

    fn observed_failure(&self) -> Option<QueryFailure> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Some(admission_failure("query acquisition exceeded its deadline"));
        }
        (self.cancelled)().then_some(QueryFailure::Cancelled)
    }
}

fn admission_failure(detail: impl std::fmt::Display) -> QueryFailure {
    QueryFailure::Unavailable {
        operation: "query acquisition".to_owned(),
        detail: detail.to_string(),
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
enum QueryOutput {
    Plan(Box<QueryPlan>),
    Answer(QueryAnswer),
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
enum QueryFailure {
    Invalid { violation: String },
    Unsupported { capability: String },
    Unavailable { operation: String, detail: String },
    Cancelled,
}

impl QueryFailure {
    fn capture(error: &RiftError) -> Self {
        let field = |name| {
            error
                .context()
                .find_map(|(key, value)| (key == name).then_some(value.clone()))
        };
        match error.slug().as_str() {
            "rift.server.read_invalid" => Self::Invalid {
                violation: field("violation").unwrap_or_else(|| error.to_string()),
            },
            "rift.server.read_cancelled" => Self::Cancelled,
            "rift.server.read_unsupported" => Self::Unsupported {
                capability: field("capability").unwrap_or_else(|| error.to_string()),
            },
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
            Self::Unsupported { capability } => errors::server::read_unsupported()
                .capability(capability)
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
    let task = QueryExchange::new(task);
    let written = write_json(
        RequestWriter {
            writer: &mut request,
            active: &active,
        },
        &task,
        process_limits.input_bytes_max,
    );
    check_request(deadline, cancelled)?;
    written?;
    drop(task);
    let mut command = Command::new(program);
    command
        .arg(QUERY_CHILD_COMMAND)
        .arg(request.path())
        .arg(response.path())
        .arg(process_limits.input_bytes_max.to_string())
        .arg(process_limits.output_bytes_max.to_string())
        .arg(process_limits.heap_bytes_max.to_string());
    let timeout = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| unavailable("query exceeded the request deadline"))?;
    let run = run_bounded_streams(
        &mut command,
        timeout,
        ACKNOWLEDGMENT_BYTES_MAX,
        process_limits.diagnostic_bytes_max,
        cancelled,
    )
    .map_err(process_error)?;
    collect_response(
        run,
        &response,
        process_limits.output_bytes_max,
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
    if run.stdout.total_bytes > u64::try_from(ACKNOWLEDGMENT_BYTES_MAX).unwrap_or(u64::MAX) {
        return Err(incompatible(
            "query protocol acknowledgment exceeds its frame bound",
        ));
    }
    decode_acknowledgment(run.stdout.text.as_bytes())?;
    if !exit.success() {
        return Err(unavailable(format!(
            "query child exited unsuccessfully: {exit}"
        )));
    }
    let bytes = read_request(response.reopen().map_err(process_error)?, bytes_max)?;
    check_request(deadline, cancelled)?;
    let response = decode_response(&bytes);
    check_request(deadline, cancelled)?;
    response
}

fn decode_response(bytes: &[u8]) -> Result<QueryOutput, RiftError> {
    let response: QueryExchange<QueryResponse> = serde_json::from_slice(bytes)
        .map_err(|_| incompatible("query response schema is incompatible"))?;
    response.into_payload()?.map_err(QueryFailure::into_error)
}

fn decode_acknowledgment(bytes: &[u8]) -> Result<(), RiftError> {
    let acknowledgment: QueryExchange<()> = serde_json::from_slice(bytes)
        .map_err(|_| incompatible("query protocol acknowledgment is missing or invalid"))?;
    acknowledgment.into_payload()
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
    input_bytes_max: usize,
    output_bytes_max: usize,
    heap_bytes_max: usize,
) -> Result<(), RiftError> {
    write_json(
        std::io::stdout(),
        &QueryExchange::new(()),
        ACKNOWLEDGMENT_BYTES_MAX,
    )?;
    let request = std::fs::File::open(request_path).map_err(process_error)?;
    let bytes = read_request(request, input_bytes_max)?;
    let request = serde_json::from_slice::<QueryExchange<QueryTask>>(&bytes)
        .map_err(|_| incompatible("query request schema is incompatible"))
        .and_then(QueryExchange::into_payload)
        .and_then(|task| {
            let limits = match &task {
                QueryTask::Prepare { limits, .. } | QueryTask::Evaluate { limits, .. } => *limits,
            };
            let accepted = QueryProcessLimits::from_query(limits)?;
            if (
                accepted.input_bytes_max,
                accepted.output_bytes_max,
                accepted.heap_bytes_max,
            ) != (input_bytes_max, output_bytes_max, heap_bytes_max)
            {
                return Err(incompatible(
                    "query child limits differ from the accepted request",
                ));
            }
            Ok(task)
        });
    drop(bytes);
    let response = QueryExchange::new(
        request
            .and_then(run_task)
            .map_err(|error| QueryFailure::capture(&error)),
    );
    let output = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(response_path)
        .map_err(process_error)?;
    write_json(output, &response, output_bytes_max)
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

fn incompatible(capability: &'static str) -> RiftError {
    errors::server::read_unsupported()
        .capability(capability)
        .error()
}

#[cfg(test)]
mod tests;
