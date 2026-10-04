//! The server's own records, stored in the metrics database at `.rift/metrics`.
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

mod reads;
mod record;
mod store;

pub use reads::{LogReader, LogReads};
pub use record::{
    LOG_BATCH_RECORDS_MAX, LOG_FIELDS_BYTES_MAX, LOG_LABEL_BYTES_MAX, LOG_LEVELS,
    LOG_MESSAGE_BYTES_MAX, LOG_PAGE_RECORDS_MAX, LogQuery, LogRecord, StoredLogRecord,
};
pub use store::{LogStore, METRICS_BUSY_TIMEOUT_MS, METRICS_SCHEMA_VERSION, WalCheckpoint};
