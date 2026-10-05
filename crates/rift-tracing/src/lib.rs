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
//! [`TracingRuntime::shutdown`]. [`LogRecord::rendered`] prints a record the way
//! `rift server logs` shows it.
//!
//! A test captures what the code under test records through a `ScopedRecorder`, which the
//! `fixtures` feature compiles in; dependent crates enable it from their
//! dev-dependencies only, so a release build carries no recorder.

mod capture;
mod drain;
mod measurement;
mod otlp;
mod reads;
mod record;
#[cfg(any(test, feature = "fixtures"))]
mod recorder;
mod render;
mod runtime;
mod span;
mod stderr;
mod store;
mod traced;

pub use capture::{
    LOG_QUEUE_RECORDS, LogSink, PANIC_PAYLOAD_BYTES_MAX, install_panic_hook, log_capture,
};
pub use drain::{LOG_SETTLE_TIMEOUT, LogDrain, LogLane, RunningLogDrain, settle_for_read};
pub use measurement::{ClockRegression, PerformanceMeasurement};
pub use reads::{LogReader, LogReads};
pub use record::{
    LOG_BATCH_RECORDS_MAX, LOG_FIELDS_BYTES_MAX, LOG_LABEL_BYTES_MAX, LOG_LEVELS,
    LOG_MESSAGE_BYTES_MAX, LOG_PAGE_RECORDS_MAX, LogQuery, LogRecord, StoredLogRecord,
};
#[cfg(any(test, feature = "fixtures"))]
pub use recorder::{
    SCOPED_RECORDER_PRINT_BYTES_MAX, SCOPED_RECORDER_PRINT_RECORDS_MAX, ScopedRecorder,
    ScopedRecorderBuilder,
};
pub use runtime::{
    LogFilterError, StderrPolicy, TracingRuntime, TracingRuntimeBuilder, validate_log_filter,
};
pub use span::Span;
pub use stderr::SERVER_STDERR_BYTES_MAX;
pub use store::{LogStore, METRICS_BUSY_TIMEOUT_MS, METRICS_SCHEMA_VERSION, WalCheckpoint};
pub use tracing::{debug, error, info, trace, warn};

/// Items the exported macros expand to. Application code never names them.
#[doc(hidden)]
pub mod __private {
    pub use crate::measurement::monotonic_now;
    pub use crate::span::span_from;
    pub use crate::traced::{parent_span, traced_future};
    pub use tracing;
}
