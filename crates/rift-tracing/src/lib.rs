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

mod measurement;
mod reads;
mod record;
mod span;
mod store;
mod traced;

pub use measurement::{ClockRegression, PerformanceMeasurement};
pub use reads::{LogReader, LogReads};
pub use record::{
    LOG_BATCH_RECORDS_MAX, LOG_FIELDS_BYTES_MAX, LOG_LABEL_BYTES_MAX, LOG_LEVELS,
    LOG_MESSAGE_BYTES_MAX, LOG_PAGE_RECORDS_MAX, LogQuery, LogRecord, StoredLogRecord,
};
pub use span::Span;
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
