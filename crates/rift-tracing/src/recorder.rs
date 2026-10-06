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
//! panics, so a failed assertion carries what the code recorded before it. A test that
//! nextest ends at its timeout never unwinds, so that print never runs: with
//! [`SCOPED_RECORDER_STREAM_VARIABLE`] set, the recorder prints each record to standard
//! error as it is recorded instead, and nextest's captured stderr holds them at the kill.
//!
//! Metrics are the process's: the first recorder installs the process's meter, and
//! [`ScopedRecorder::metrics`] reads what the OpenTelemetry SDK exports from it.

mod metrics;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tracing_subscriber::layer::SubscriberExt as _;

use crate::capture::log_capture;
use crate::drain::LogDrain;
use crate::flight::{FlightLayer, FlightTable, observe_active};
use crate::metrics::ObservationGuard;
use crate::otlp::SDK_TARGET;
use crate::record::LogRecord;
use crate::render::LogLines;
use crate::runtime::{LogFilterError, capture_layer, parsed_filter};

pub use self::metrics::{MetricSeries, MetricSnapshot, SeriesValue};

/// Most records a panicking test's recorder prints: the newest ones it captured.
pub const SCOPED_RECORDER_PRINT_RECORDS_MAX: usize = 256;
/// Most bytes of rendered records a panicking test's recorder prints. The newest records
/// that fit are printed; the count of the earlier ones left out is printed once, first.
pub const SCOPED_RECORDER_PRINT_BYTES_MAX: usize = 64 << 10;

/// The environment variable that makes every recorder print each record to standard error
/// as it is recorded, in the live stream's line, and no panic print. The nextest runner
/// (`dev/src/rift_dev/nextest_run.py`) sets it. Nextest runs each test binary with
/// `--nocapture`, so the lines reach the stderr nextest captures as they print: on a
/// timeout nextest sends `SIGTERM`, then `SIGKILL` after the grace period on Unix, and
/// kills the job object at once on Windows, and either way the lines printed before the
/// kill stay in its output. Unset, a recorder prints only when its test panics.
pub const SCOPED_RECORDER_STREAM_VARIABLE: &str = "RIFT_SCOPED_RECORDER_STREAM";

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
/// [`SCOPED_RECORDER_PRINT_BYTES_MAX`]. A test that passes prints nothing, unless
/// [`SCOPED_RECORDER_STREAM_VARIABLE`] is set: then every record prints as it is recorded.
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
    output: PanicOutput,
    /// Keeps the recorder's table of operations in flight reported in `operation.active`.
    _in_flight: Option<ObservationGuard>,
    _default: tracing::subscriber::DefaultGuard,
}

impl ScopedRecorder {
    /// A builder whose recorder captures every level of every target.
    pub fn builder() -> ScopedRecorderBuilder {
        ScopedRecorderBuilder {
            capture: None,
            stream: std::env::var_os(SCOPED_RECORDER_STREAM_VARIABLE).is_some(),
        }
    }

    /// Every series the process's instruments hold now, as the OpenTelemetry SDK exports
    /// them: every value recorded on any thread since the process's first recorder
    /// installed its meter, whichever recorder is held.
    ///
    /// ```
    /// let (recorder, _drain) = rift_tracing::ScopedRecorder::builder().install()?;
    /// rift_tracing::traced!("index.parse", 1 + 1);
    /// let calls = recorder.metrics();
    /// let parsed = calls.find(
    ///     "traces.span.metrics.calls",
    ///     &[("span.name", "index.parse"), ("span.kind", "Internal"), ("status.code", "Ok")],
    /// );
    /// assert_eq!(parsed.map(|series| series.value().clone()), Some(rift_tracing::SeriesValue::Sum(1.0)));
    /// # Ok::<(), rift_tracing::LogFilterError>(())
    /// ```
    #[must_use]
    pub fn metrics(&self) -> MetricSnapshot {
        metrics::snapshot()
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
        // A streaming recorder printed every record already.
        if !std::thread::panicking() || self.retained.stream.is_some() {
            return;
        }
        self.output.print(&self.retained.printed());
    }
}

/// The settings [`ScopedRecorderBuilder::install`] builds the recorder from.
#[derive(Debug)]
#[must_use = "a builder installs nothing until `install` runs"]
pub struct ScopedRecorderBuilder {
    capture: Option<String>,
    /// Whether [`SCOPED_RECORDER_STREAM_VARIABLE`] was set when the builder was made.
    stream: bool,
}

impl ScopedRecorderBuilder {
    /// Captures what `filter` admits, in the `RUST_LOG` spelling
    /// [`TracingRuntimeBuilder::capture`](crate::TracingRuntimeBuilder::capture) takes.
    /// Without it the recorder captures every level of every target. Either way the
    /// OpenTelemetry SDK's own reports are captured at `WARN` and above, as stderr carries
    /// them: the SDK reports each instrument it builds at `DEBUG`.
    pub fn capture(mut self, filter: &str) -> Self {
        self.capture = Some(filter.to_owned());
        self
    }

    /// Streams each record, or prints only on a panic, whatever the environment says.
    #[cfg(test)]
    pub(crate) fn stream(mut self, stream: bool) -> Self {
        self.stream = stream;
        self
    }

    /// Installs the recorder as the calling thread's default subscriber, and the process's
    /// meter unless one is installed, and returns the recorder with the drain its records
    /// reach.
    ///
    /// # Errors
    ///
    /// Returns [`LogFilterError`] when the [`Self::capture`] filter does not parse.
    pub fn install(self) -> Result<(ScopedRecorder, LogDrain), LogFilterError> {
        let mut filter =
            parsed_filter(self.capture.as_deref().unwrap_or(RECORDER_DEFAULT_CAPTURE))?;
        if let Ok(reports) = format!("{SDK_TARGET}=warn").parse() {
            filter = filter.add_directive(reports);
        }
        let stream = self.stream.then_some(PanicOutput::Stderr);
        let retained = Arc::new(RetainedRecords {
            stream,
            ..RetainedRecords::default()
        });
        let (sink, drain) = log_capture();
        let sink = sink.retaining(Arc::clone(&retained));
        metrics::install();
        let flights = Arc::new(FlightTable::default());
        let in_flight = observe_active(&flights);
        let subscriber = crate::capture::registry()
            .with(FlightLayer::new(flights))
            .with(capture_layer(sink, filter));
        let recorder = ScopedRecorder {
            retained,
            output: PanicOutput::Stderr,
            _in_flight: in_flight,
            _default: tracing::subscriber::set_default(subscriber),
        };
        Ok((recorder, drain))
    }
}

/// Where a recorder prints: its panic print, and the records it streams.
#[derive(Debug)]
pub(crate) enum PanicOutput {
    Stderr,
    #[cfg(test)]
    Buffer(Arc<Mutex<String>>),
}

impl PanicOutput {
    fn print(&self, text: &str) {
        match self {
            // `eprint!` reaches the test harness's output capture; a direct write to the
            // stderr handle would bypass it under `cargo test`.
            Self::Stderr => eprint!("{text}"),
            #[cfg(test)]
            Self::Buffer(buffer) => buffer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_str(text),
        }
    }
}

/// The newest records a recorder captured, kept for the print a panic triggers.
#[derive(Debug, Default)]
pub(crate) struct RetainedRecords {
    records: Mutex<VecDeque<LogRecord>>,
    /// Records pushed out by newer ones once [`SCOPED_RECORDER_PRINT_RECORDS_MAX`] were kept.
    left_out: AtomicU64,
    /// Where each record prints as it is kept, when [`SCOPED_RECORDER_STREAM_VARIABLE`]
    /// was set at the install.
    pub(crate) stream: Option<PanicOutput>,
}

impl RetainedRecords {
    /// Keeps a copy of `record`, leaving out the oldest kept record past the bound, and
    /// prints its live stream line when the recorder streams.
    pub(crate) fn keep(&self, record: &LogRecord) {
        {
            let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
            records.push_back(record.clone());
            if records.len() > SCOPED_RECORDER_PRINT_RECORDS_MAX {
                records.pop_front();
                self.left_out.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some(stream) = &self.stream {
            stream.print(&format!("{}\n", record.rendered()));
        }
    }

    /// The text a panic prints: one line stating the records captured and left out, then
    /// the newest records that fit [`SCOPED_RECORDER_PRINT_BYTES_MAX`], oldest first, as
    /// the stored page `rift server logs` prints, in UTC. A record fits by the length of
    /// its live stream line.
    fn printed(&self) -> String {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let mut kept = 0;
        let mut bytes = 0;
        for record in records.iter().rev() {
            let length = record.rendered().len() + 1;
            if bytes + length > SCOPED_RECORDER_PRINT_BYTES_MAX {
                break;
            }
            bytes += length;
            kept += 1;
        }
        let left_out = self.left_out.load(Ordering::Relaxed)
            + u64::try_from(records.len() - kept).unwrap_or(u64::MAX);
        let newest = records
            .iter()
            .skip(records.len() - kept)
            .cloned()
            .collect::<Vec<_>>();
        let mut printed = format!(
            "scoped recorder: {kept} records printed, {left_out} earlier records left out\n"
        );
        printed.push_str(&LogLines::stored_page().lines(&newest));
        printed
    }
}

#[cfg(test)]
mod tests;
