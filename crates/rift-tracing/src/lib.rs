//! The server's own records, and the instrumentation every Rift crate emits them through.
//!
//! A server whose index will not settle answers no read, and its diagnostics
//! leave on stderr, where the agent asking the question cannot reach them. The
//! rows here are what an agent reads back through `rift://logs` while that
//! index lane is still stuck, so the run explains itself without an operator
//! copying a terminal.
//!
//! The metrics database is a file of its own. Its writer runs on one thread
//! with one connection, and every read opens a connection of its own, so a log
//! write or read reaches no queue, pool, or lock of the index database.
//!
//! Crates time an operation with [`traced!`], hold its context as a [`Span`],
//! emit events with [`trace!`], [`debug!`], [`info!`], [`warn!`], and
//! [`error!`], and measure elapsed time they act on with [`measure_elapsed!`]. The event macros keep the caller's
//! module path as the event target, so a filter such as `rift_mcp=debug`
//! selects the same callers.
//!
//! A serving process records what its filter admits: [`log_capture`] builds the
//! [`LogSink`] layer and its [`LogDrain`], and [`RunningLogDrain`] writes the queue into
//! the metrics database until a stop joins it.
//!
//! A binary starts tracing once through [`TracingRuntime::builder`], which installs stderr,
//! the capture, and the optional OTLP export under filters of their own, and stops it with
//! [`TracingRuntime::shutdown`]. [`LogLines`] prints records the way stderr and
//! `rift server logs` show them.
//!
//! Code records values into typed instruments: [`metrics`] returns the ones Rift declares,
//! and [`Counter`], [`Gauge`], and [`Histogram`] declare more. Every [`traced!`] operation
//! records its duration and outcome, and the runtime's process sampler publishes the
//! process's memory, CPU, open files, and disk bytes. [`TracingRuntime::metrics`] reads
//! every value in process, in every build.
//!
//! Work still running is evidence too. Every operation, and every wait for and hold of a
//! lock recorded through [`lock`], stays in the table of operations in flight until it
//! closes; [`publish_in_flight`] writes the table as one record, and the process sampler
//! reports each entry open past `[logs] stall_delay` and writes metric snapshot records,
//! [`RecordKind::Metric`], into the same stream. [`LogQuery`] selects a window of either
//! kind.
//!
//! A test captures what the code under test records through a `ScopedRecorder`, which the
//! `fixtures` feature compiles in; dependent crates enable it from their
//! dev-dependencies only, so a release build carries no recorder.

// `#[timed]` names this crate `::rift_tracing` in every expansion, including the ones inside
// it, its unit tests, and its doctests.
extern crate self as rift_tracing;

mod capture;
mod drain;
mod flight;
mod lock;
mod measurement;
mod metrics;
mod otlp;
mod reads;
mod record;
#[cfg(any(test, feature = "fixtures"))]
mod recorder;
mod render;
mod runtime;
mod sampler;
mod snapshot;
mod span;
mod stderr;
mod store;
mod traced;

pub use capture::{
    LOG_QUEUE_RECORDS, LogSink, PANIC_PAYLOAD_BYTES_MAX, install_panic_hook, log_capture,
};
pub use drain::{LOG_SETTLE_TIMEOUT, LogDrain, LogLane, RunningLogDrain, settle_for_read};
pub use flight::{OPERATIONS_IN_FLIGHT_MAX, publish_in_flight, warn_in_flight};
pub use lock::{Acquire, Held, Lock, Refusal, lock};
pub use measurement::{ClockRegression, PerformanceMeasurement};
pub use metrics::{
    Counter, CounterSelection, DURATION_BOUNDARIES_SECONDS, Gauge, GaugeSelection, GaugeValue,
    HISTOGRAM_BOUNDARIES_MAX, Histogram, HistogramSelection, Instrument, InstrumentKind,
    METRIC_LABELS_MAX, METRIC_SERIES_MAX_DEFAULT, MetricSeries, MetricSnapshot, Metrics,
    ProcessGauge, SeriesValue, metrics,
};
pub use reads::{LogReader, LogReads};
pub use record::{
    LOG_BATCH_RECORDS_MAX, LOG_FIELDS_BYTES_MAX, LOG_LABEL_BYTES_MAX, LOG_LEVELS,
    LOG_MESSAGE_BYTES_MAX, LOG_PAGE_RECORDS_MAX, LogQuery, LogRecord, RecordKind, StoredLogRecord,
};
#[cfg(any(test, feature = "fixtures"))]
pub use recorder::{
    SCOPED_RECORDER_PRINT_BYTES_MAX, SCOPED_RECORDER_PRINT_RECORDS_MAX, ScopedRecorder,
    ScopedRecorderBuilder,
};
pub use render::LogLines;
pub use runtime::{
    InstallError, LogFilterError, StderrPolicy, TracingRuntime, TracingRuntimeBuilder,
    validate_log_filter,
};
pub use sampler::{PROCESS_SAMPLE_INTERVAL_MIN, SAMPLE_HOOKS_MAX, SampleHook, sample_hook};
pub use span::Span;
pub use stderr::SERVER_STDERR_BYTES_MAX;
pub use store::{
    LogStore, METRICS_BUSY_TIMEOUT_MS, METRICS_SCHEMA_VERSION, StoreClose, WalCheckpoint,
};

/// Times a whole function as one [`traced!`] operation.
///
/// The arguments are those of `traced!`: the operation literal first, then optionally
/// `component = <expr>` and extra `name = value` fields after it. An `async fn` runs its
/// body through the async form, so its operation starts on the first poll and records a
/// cancellation when the future is dropped before it returns; any other function runs its
/// body through the block form.
///
/// ```
/// #[rift_tracing::timed("package.analyze", component = "dependency", sources = sources.len())]
/// fn analyze(sources: &[&str]) -> Result<usize, String> {
///     if sources.is_empty() {
///         return Err("no source".to_owned());
///     }
///     Ok(sources.len())
/// }
/// assert_eq!(analyze(&["lib.rs"]), Ok(1));
///
/// struct Store {
///     name: String,
/// }
///
/// impl Store {
///     #[rift_tracing::timed("lexical.commit")]
///     async fn commit(&self, documents: usize) -> (&str, usize) {
///         (&self.name, documents)
///     }
/// }
/// # let store = Store { name: "index".to_owned() };
/// # let _ = store.commit(2);
/// ```
///
/// # Evaluation and control flow
///
/// - The signature, generics, visibility, attributes, and return value stay as written.
/// - The function's arguments are not recorded. A field names what the span carries, and
///   evaluates once when the operation starts, as in `traced!`.
/// - `return` and `?` leave the function, and the operation finishes on every path out.
/// - A function that returns a future without being `async` is timed until it returns
///   the future.
///
/// A `const fn` is refused at compile time, because the clock and the span run when the
/// function is called:
///
/// ```compile_fail
/// #[rift_tracing::timed("index.parse")]
/// const fn parse() -> u8 {
///     1
/// }
/// ```
///
/// A computed operation name is refused at compile time:
///
/// ```compile_fail
/// const OPERATION: &str = "index.parse";
/// #[rift_tracing::timed(OPERATION)]
/// fn parse() -> u8 {
///     1
/// }
/// ```
///
/// A field needs `component` before it, as `traced!`'s detailed form does:
///
/// ```compile_fail
/// #[rift_tracing::timed("index.parse", units = 1)]
/// fn parse() -> u8 {
///     1
/// }
/// ```
pub use rift_tracing_macros::timed;

/// Items the exported macros expand to. Application code never names them.
#[doc(hidden)]
pub mod __private {
    pub use crate::measurement::monotonic_now;
    pub use crate::metrics::{Completion, completion};
    pub use crate::span::{function_name, span_from};
    pub use crate::traced::{parent_span, traced_future};
    pub use tracing;
}
