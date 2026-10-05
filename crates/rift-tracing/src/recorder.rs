//! `ScopedRecorder`: a test's own log capture, installed on the calling thread.
//!
//! A test that asserts on what the code under test recorded needs a subscriber, and a
//! process-global one is shared by every test in the binary. The recorder installs the
//! capture layer [`TracingRuntime`](crate::TracingRuntime) installs, under the filter the
//! test names, as the calling thread's default until the recorder drops. The records reach
//! the [`LogDrain`] the recorder returns, the representation a serving process writes into
//! the metrics database.
//!
//! The recorder also keeps the newest records it captured, and prints them when its test
//! panics, so a failed assertion carries what the code recorded before it.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use jiff::tz::TimeZone;
use tracing_subscriber::layer::SubscriberExt as _;

use crate::capture::log_capture;
use crate::drain::LogDrain;
use crate::flight::{FlightLayer, FlightTable};
use crate::metrics::{MetricLayer, MetricSnapshot, MetricValues};
use crate::record::LogRecord;
use crate::runtime::{LogFilterError, capture_layer, parsed_filter};

/// Most records a panicking test's recorder prints: the newest ones it captured.
pub const SCOPED_RECORDER_PRINT_RECORDS_MAX: usize = 256;
/// Most bytes of rendered records a panicking test's recorder prints. The newest records
/// that fit are printed; the count of the earlier ones left out is printed once, first.
pub const SCOPED_RECORDER_PRINT_BYTES_MAX: usize = 64 << 10;

/// The filter a recorder captures under when its builder names none: every level of
/// every target.
const RECORDER_DEFAULT_CAPTURE: &str = "trace";

/// A test's log capture, installed as the calling thread's default subscriber while held.
///
/// Records emitted on the installing thread, and by tasks a current-thread runtime polls
/// there, reach the [`LogDrain`] returned beside it. A thread the test spawns runs under
/// its own default and records nothing here. Recorders nest: dropping the inner one
/// restores the outer one, so nested recorders drop in reverse order of installation, as
/// locals in one scope do.
///
/// When its test panics, the recorder prints the newest records it captured to standard
/// error, bounded by [`SCOPED_RECORDER_PRINT_RECORDS_MAX`] and
/// [`SCOPED_RECORDER_PRINT_BYTES_MAX`]. A test that passes prints nothing.
///
/// ```
/// let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
///     .capture("info")
///     .install()?;
/// rift_tracing::info!(component = "index", operation = "index.publish", "published");
/// rift_tracing::debug!("below the filter");
/// drop(recorder);
///
/// let records = drain.queued_records();
/// assert_eq!(records.len(), 1);
/// assert_eq!(records[0].message(), "published");
/// assert_eq!(records[0].operation(), "index.publish");
/// # Ok::<(), rift_tracing::LogFilterError>(())
/// ```
#[derive(Debug)]
#[must_use = "the recorder captures only while it is held"]
pub struct ScopedRecorder {
    retained: Arc<RetainedRecords>,
    values: Arc<MetricValues>,
    output: PanicOutput,
    _default: tracing::subscriber::DefaultGuard,
}

impl ScopedRecorder {
    /// A builder whose recorder captures every level of every target.
    pub fn builder() -> ScopedRecorderBuilder {
        ScopedRecorderBuilder { capture: None }
    }

    /// Every value the code under test recorded into an instrument on this thread.
    ///
    /// ```
    /// let (recorder, _drain) = rift_tracing::ScopedRecorder::builder().install()?;
    /// rift_tracing::traced!("index.parse", 1 + 1);
    /// let calls = recorder.metrics();
    /// let parsed = calls.find(
    ///     "traces.span.metrics.calls",
    ///     &[("span.name", "index.parse"), ("status.code", "Ok")],
    /// );
    /// assert_eq!(parsed.map(|series| series.value().clone()), Some(rift_tracing::SeriesValue::Sum(1.0)));
    /// # Ok::<(), rift_tracing::LogFilterError>(())
    /// ```
    #[must_use]
    pub fn metrics(&self) -> MetricSnapshot {
        self.values.snapshot()
    }

    /// The metric values the recorder holds, for a test that publishes a process sample.
    #[cfg(test)]
    pub(crate) fn values(&self) -> &MetricValues {
        &self.values
    }

    /// Prints into `buffer` instead of standard error, so a test can read what a panic
    /// printed.
    #[cfg(test)]
    pub(crate) fn print_into(&mut self, buffer: Arc<Mutex<String>>) {
        self.output = PanicOutput::Buffer(buffer);
    }
}

impl Drop for ScopedRecorder {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let printed = self.retained.printed();
        match &self.output {
            // `eprint!` reaches the test harness's output capture; a direct write to the
            // stderr handle would bypass it under `cargo test`.
            PanicOutput::Stderr => eprint!("{printed}"),
            #[cfg(test)]
            PanicOutput::Buffer(buffer) => buffer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_str(&printed),
        }
    }
}

/// The settings [`ScopedRecorderBuilder::install`] builds the recorder from.
#[derive(Debug)]
#[must_use = "a builder installs nothing until `install` runs"]
pub struct ScopedRecorderBuilder {
    capture: Option<String>,
}

impl ScopedRecorderBuilder {
    /// Captures what `filter` admits, in the `RUST_LOG` spelling
    /// [`TracingRuntimeBuilder::capture`](crate::TracingRuntimeBuilder::capture) takes.
    /// Without it the recorder captures every level of every target.
    pub fn capture(mut self, filter: &str) -> Self {
        self.capture = Some(filter.to_owned());
        self
    }

    /// Installs the recorder as the calling thread's default subscriber, and returns it
    /// with the drain its records reach.
    ///
    /// # Errors
    ///
    /// Returns [`LogFilterError`] when the [`Self::capture`] filter does not parse.
    pub fn install(self) -> Result<(ScopedRecorder, LogDrain), LogFilterError> {
        let filter = parsed_filter(self.capture.as_deref().unwrap_or(RECORDER_DEFAULT_CAPTURE))?;
        let retained = Arc::new(RetainedRecords::default());
        let (sink, drain) = log_capture();
        let sink = sink.retaining(Arc::clone(&retained));
        let values = Arc::new(MetricValues::default());
        let subscriber = tracing_subscriber::registry()
            .with(MetricLayer::new(Arc::clone(&values)))
            .with(FlightLayer::new(Arc::new(FlightTable::default())))
            .with(capture_layer(sink, filter));
        let recorder = ScopedRecorder {
            retained,
            values,
            output: PanicOutput::Stderr,
            _default: tracing::subscriber::set_default(subscriber),
        };
        Ok((recorder, drain))
    }
}

/// Where a panicking test's recorder prints.
#[derive(Debug)]
enum PanicOutput {
    Stderr,
    #[cfg(test)]
    Buffer(Arc<Mutex<String>>),
}

/// The newest records a recorder captured, kept for the print a panic triggers.
#[derive(Debug, Default)]
pub(crate) struct RetainedRecords {
    records: Mutex<VecDeque<LogRecord>>,
    /// Records pushed out by newer ones once [`SCOPED_RECORDER_PRINT_RECORDS_MAX`] were kept.
    left_out: AtomicU64,
}

impl RetainedRecords {
    /// Keeps a copy of `record`, leaving out the oldest kept record past the bound.
    pub(crate) fn keep(&self, record: &LogRecord) {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        records.push_back(record.clone());
        if records.len() > SCOPED_RECORDER_PRINT_RECORDS_MAX {
            records.pop_front();
            self.left_out.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The text a panic prints: one line stating the records captured and left out, then
    /// the newest records that fit [`SCOPED_RECORDER_PRINT_BYTES_MAX`], oldest first, each
    /// rendered the way `rift server logs` prints it, in UTC.
    fn printed(&self) -> String {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let mut lines = Vec::new();
        let mut bytes = 0;
        for record in records.iter().rev() {
            let line = record.rendered(&TimeZone::UTC);
            if bytes + line.len() + 1 > SCOPED_RECORDER_PRINT_BYTES_MAX {
                break;
            }
            bytes += line.len() + 1;
            lines.push(line);
        }
        let left_out = self.left_out.load(Ordering::Relaxed)
            + u64::try_from(records.len() - lines.len()).unwrap_or(u64::MAX);
        let mut printed = format!(
            "scoped recorder: {} records printed, {left_out} earlier records left out\n",
            lines.len()
        );
        for line in lines.iter().rev() {
            let _ = writeln!(printed, "{line}");
        }
        printed
    }
}

#[cfg(test)]
mod tests;
