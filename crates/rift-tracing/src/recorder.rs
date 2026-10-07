//! `ScopedRecorder`: a test's own log capture, installed on the calling thread.
//!
//! A test that asserts on what the code under test recorded needs a subscriber, and a
//! process-global one is shared by every test in the binary. The recorder installs the
//! capture layer [`TracingRuntime`](crate::TracingRuntime) installs, under the filter the
//! test names, as the calling thread's default until the recorder drops. The records reach
//! the [`LogDrain`] the recorder returns, the representation a serving process writes into
//! the metrics database.
//!
//! A test that installs no recorder has no thread-local subscriber. Under
//! [`SCOPED_RECORDER_STREAM_VARIABLE`], the first record, span, or lock in a process
//! started by nextest installs a process-wide default subscriber and OTLP exporter. The
//! process records until it installs a recorder, and nothing after: the recorder takes
//! its own thread's records, since `tracing-core` uses the global default only "as a
//! fallback if no thread-local dispatch has been set in a thread". Every
//! [`UNSCOPED_IN_FLIGHT_INTERVAL`], the process also records its operations still in flight.
//!
//! Metrics are the process's: the first recorder installs the process's meter, and
//! [`ScopedRecorder::metrics`] reads what the OpenTelemetry SDK exports from it.
//!
//! Span timings and [`monotonic_now`](crate::__private::monotonic_now) on the recorder's
//! thread read the clock [`ScopedRecorderBuilder::clock`] names, so a test moves time by
//! hand instead of sleeping.

mod metrics;
mod unscoped;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tracing_subscriber::layer::SubscriberExt as _;

use crate::capture::{SpanContextLayer, log_capture};
use crate::drain::LogDrain;
use crate::flight::{FlightLayer, FlightTable, observe_active};
use crate::measurement::monotonic_now;
use crate::metrics::ObservationGuard;
use crate::otlp::{self, ExportShutdownError, OtlpExport, SDK_TARGET};
use crate::runtime::{LogFilterError, capture_layer, parsed_filter};

pub use self::metrics::{MetricSeries, MetricSnapshot, SeriesValue};

/// The environment variable that enables the unscoped test-process exporter.
/// `dev/src/rift_dev/nextest_run.py` sets it for each test process.
pub const SCOPED_RECORDER_STREAM_VARIABLE: &str = "RIFT_SCOPED_RECORDER_STREAM";

/// How often the unscoped test process records operations still in flight, while any is.
pub const UNSCOPED_IN_FLIGHT_INTERVAL: Duration = Duration::from_secs(5);

/// The argument nextest passes every test it runs: it runs `<binary> --exact <name>
/// --nocapture` (`nextest-runner/src/list/test_list.rs`, 0.9.145). A `rift` process a
/// test starts never carries it, so its own [`TracingRuntime`](crate::TracingRuntime)
/// keeps the global default.
const NEXTEST_ARGUMENT: &str = "--nocapture";

/// Whether this process tried the unscoped stream already.
static UNSCOPED_TRIED: AtomicBool = AtomicBool::new(false);
/// Whether this process installed a recorder: from then on the unscoped stream enables
/// nothing (`unscoped.rs` states why).
static RECORDER_INSTALLED: AtomicBool = AtomicBool::new(false);
/// SDK providers remain open until the test process exits.
static TEST_EXPORTS: OnceLock<Mutex<Vec<OtlpExport>>> = OnceLock::new();
static TEST_METERS: OnceLock<()> = OnceLock::new();
static TEST_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
static TEST_SHUTDOWN_HOOK: OnceLock<bool> = OnceLock::new();
const TEST_EXPORTS_MAX: usize = 512;
#[cfg(test)]
static UNSCOPED_EXPORT_SHUTDOWN_SUCCEEDED: AtomicBool = AtomicBool::new(false);

/// Drives the SDK's standard asynchronous processors for synchronous libtest callers.
pub(crate) fn test_runtime() -> &'static tokio::runtime::Runtime {
    TEST_RUNTIME.get_or_init(|| {
        assert!(
            *TEST_SHUTDOWN_HOOK.get_or_init(|| shutdown_hooks::add_shutdown_hook(
                shutdown_unscoped_test_export_at_exit
            )),
            "test process export shutdown hook registers before setup"
        );
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the test OTLP runtime builds")
    })
}

pub(crate) fn retain_export(export: OtlpExport) {
    if !otlp::recorder_export_configured() {
        return;
    }
    let mut exports = TEST_EXPORTS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(
        exports.len() < TEST_EXPORTS_MAX,
        "test process SDK providers stay within their bound"
    );
    exports.push(export);
}

fn install_test_process_export(filter: tracing_subscriber::EnvFilter) {
    if !otlp::recorder_export_configured() {
        return;
    }
    TEST_METERS.get_or_init(|| {
        let _entered = test_runtime().enter();
        let (layer, export) = otlp::test_process_layer::<tracing_subscriber::Registry>(filter);
        drop(layer);
        metrics::install(
            export.recorder_meter_provider(),
            export.recorder_metric_reader(),
        );
        retain_export(export);
    });
}

/// Installs the standard OTLP layer for a test without a scoped recorder.
pub(crate) fn stream_unscoped() {
    if RECORDER_INSTALLED.load(Ordering::Relaxed)
        || UNSCOPED_TRIED.swap(true, Ordering::Relaxed)
        || std::env::var_os("NEXTEST_ATTEMPT_ID").is_none()
        || !std::env::args_os().any(|argument| argument == NEXTEST_ARGUMENT)
        || std::env::var_os(SCOPED_RECORDER_STREAM_VARIABLE).is_none()
        || !otlp::recorder_export_configured()
    {
        return;
    }
    let Ok(filter) = recorder_filter(Some(UNSCOPED_CAPTURE)) else {
        return;
    };
    let _entered = test_runtime().enter();
    let flights = Arc::new(FlightTable::default());
    let (layer, export) = otlp::test_process_layer(filter.clone());
    let subscriber = crate::capture::registry()
        .with(FlightLayer::new(Arc::clone(&flights)))
        .with(layer)
        .with(filter)
        .with(unscoped::StopAtRecorder);
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        return;
    }
    TEST_METERS.get_or_init(|| {
        metrics::install(
            export.recorder_meter_provider(),
            export.recorder_metric_reader(),
        );
    });
    retain_export(export);
    test_runtime().spawn(async move {
        let mut ticks = tokio::time::interval(UNSCOPED_IN_FLIGHT_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let listing = flights.listing(monotonic_now());
            if listing.in_flight > 0 {
                tracing::info!(target: "rift_tracing::flight", reason = "unscoped_stream",
                in_flight = listing.in_flight, left_out = listing.left_out,
                untracked = listing.untracked, operations = %listing, "operations in flight");
            }
        }
    });
}

fn shutdown_test_exports() -> Result<(), ExportShutdownError> {
    let Some(runtime) = TEST_RUNTIME.get() else {
        return Ok(());
    };
    let exports = TEST_EXPORTS
        .get()
        .map(|exports| {
            std::mem::take(
                &mut *exports
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        })
        .unwrap_or_default();
    if exports.is_empty() {
        return Ok(());
    }
    // libtest's process-exit hook is synchronous; all exporter work remains async.
    runtime.block_on(async move {
        let deadline = tokio::time::Instant::now() + crate::OTLP_SHUTDOWN_TIMEOUT;
        let mut tasks = tokio::task::JoinSet::new();
        for export in exports {
            tasks.spawn(async move { export.shutdown(deadline).await });
        }
        let mut result = Ok(());
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(error)) => result = Err(error),
                Err(error) => result = Err(ExportShutdownError::Failed(error.to_string())),
            }
        }
        result
    })
}

extern "C" fn shutdown_unscoped_test_export_at_exit() {
    // Rustdoc's main thread has destroyed Tokio's context before atexit runs.
    // Enter the retained runtime on a fresh thread; exporter work stays async.
    let result = std::thread::Builder::new()
        .name("rift-test-otlp-shutdown".to_owned())
        .spawn(shutdown_test_exports)
        .map_err(|error| ExportShutdownError::Failed(error.to_string()))
        .and_then(|worker| {
            worker.join().unwrap_or_else(|_| {
                Err(ExportShutdownError::Failed(
                    "the test process export shutdown task panicked".to_owned(),
                ))
            })
        });
    #[cfg(test)]
    UNSCOPED_EXPORT_SHUTDOWN_SUCCEEDED.store(result.is_ok(), Ordering::Release);
    if let Err(error) = result {
        eprintln!("rift: warning: {error}");
    }
}

#[cfg(test)]
pub(crate) fn shutdown_unscoped_test_export() -> Result<(), ExportShutdownError> {
    shutdown_test_exports()
}

/// The filter a recorder captures under: `capture`, or every level of every target, with
/// the OpenTelemetry SDK's own reports at `WARN` and above.
fn recorder_filter(capture: Option<&str>) -> Result<tracing_subscriber::EnvFilter, LogFilterError> {
    let mut filter = parsed_filter(capture.unwrap_or(RECORDER_DEFAULT_CAPTURE))?;
    if let Ok(reports) = format!("{SDK_TARGET}=warn").parse() {
        filter = filter.add_directive(reports);
    }
    Ok(filter)
}

/// The filter a recorder captures under when its builder names none: every level of
/// every target.
const RECORDER_DEFAULT_CAPTURE: &str = "trace";

/// The filter the unscoped stream captures under: `INFO` and above of every target, the
/// level `traced!` opens its spans at. Every level, as a recorder captures by default,
/// streamed each SQLite statement's `TRACE` and `DEBUG` records: on windows-11-arm two
/// `rift-mcp` tests with no recorder took 21.9 s and 25.5 s against 5.5 s and 8.0 s
/// without the stream (runs 37421565023 and 37416927532).
const UNSCOPED_CAPTURE: &str = "info";

/// A test's log capture, installed as the calling thread's default subscriber while held.
///
/// Records emitted on the installing thread, and by tasks a current-thread runtime polls
/// there, reach the [`LogDrain`] returned beside it. A thread the test spawns runs under
/// its own default and records nothing here. Recorders nest: dropping the inner one
/// restores the outer one, so nested recorders drop in reverse order of installation, as
/// locals in one scope do.
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
    /// Keeps the recorder's table of operations in flight reported in `operation.active`.
    _in_flight: Option<ObservationGuard>,
    _default: tracing::subscriber::DefaultGuard,
}

impl ScopedRecorder {
    /// A builder whose recorder captures every level of every target.
    pub fn builder() -> ScopedRecorderBuilder {
        ScopedRecorderBuilder {
            capture: None,
            clock: None,
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
}

/// The settings [`ScopedRecorderBuilder::install`] builds the recorder from.
#[must_use = "a builder installs nothing until `install` runs"]
pub struct ScopedRecorderBuilder {
    capture: Option<String>,
    /// The clock [`Self::clock`] named.
    clock: Option<Arc<dyn Fn() -> Duration + Send + Sync>>,
}

impl std::fmt::Debug for ScopedRecorderBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ScopedRecorderBuilder")
            .field("capture", &self.capture)
            .field("clock", &self.clock.is_some())
            .finish()
    }
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

    /// Reads `now` as the monotonic clock while the recorder is held: every span timing,
    /// and with it each `traced!` operation's duration and its close record's
    /// `elapsed_ms`, and every [`monotonic_now`](crate::__private::monotonic_now) read on
    /// the recorder's thread, such as `measure_elapsed!`'s. `now` returns the time since
    /// any fixed epoch; a test advances what it returns instead of sleeping. Without it
    /// the recorder reads the process's monotonic clock. Other threads, and other
    /// recorders, keep their own clock.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicU64, Ordering};
    /// use std::time::Duration;
    ///
    /// let elapsed_ms = Arc::new(AtomicU64::new(0));
    /// let clock = Arc::clone(&elapsed_ms);
    /// let (recorder, _drain) = rift_tracing::ScopedRecorder::builder()
    ///     .clock(move || Duration::from_millis(clock.load(Ordering::Relaxed)))
    ///     .install()?;
    /// let ((), measurement) = rift_tracing::measure_elapsed!("index.parse", {
    ///     elapsed_ms.fetch_add(250, Ordering::Relaxed);
    /// })
    /// .expect("the clock does not regress");
    /// assert_eq!(measurement.elapsed(), Duration::from_millis(250));
    /// drop(recorder);
    /// # Ok::<(), rift_tracing::LogFilterError>(())
    /// ```
    pub fn clock(mut self, now: impl Fn() -> Duration + Send + Sync + 'static) -> Self {
        self.clock = Some(Arc::new(now));
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
        let filter = recorder_filter(self.capture.as_deref())?;
        RECORDER_INSTALLED.store(true, Ordering::Relaxed);
        #[cfg(any(test, feature = "fixtures"))]
        install_test_process_export(filter.clone());
        let (sink, drain) = log_capture();
        let _entered = otlp::recorder_export_configured().then(|| test_runtime().enter());
        let (otlp_layer, export) = otlp::recorder_layer(filter.clone());
        metrics::install(None, None);
        retain_export(export);
        let flights = Arc::new(FlightTable::default());
        let in_flight = observe_active(&flights);
        let span_context = self
            .clock
            .map_or_else(SpanContextLayer::default, SpanContextLayer::with_clock);
        let subscriber = tracing_subscriber::registry()
            .with(span_context)
            .with(FlightLayer::new(flights))
            .with(capture_layer(sink, filter))
            .with(otlp_layer);
        let recorder = ScopedRecorder {
            _in_flight: in_flight,
            _default: tracing::subscriber::set_default(subscriber),
        };
        Ok((recorder, drain))
    }
}

#[cfg(test)]
mod tests;
