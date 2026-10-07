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

#[cfg(test)]
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(any(test, feature = "fixtures"))]
use opentelemetry_otlp::MetricExporter;
#[cfg(any(test, feature = "fixtures"))]
use opentelemetry_sdk::metrics::data::ResourceMetrics;
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
/// Owns test-process exporters until the nextest child exits.
#[cfg(any(test, feature = "fixtures"))]
static UNSCOPED_TEST_EXPORT: OnceLock<Mutex<Option<TestOtlpExport>>> = OnceLock::new();

/// Installs the unscoped exporter once per process, when [`SCOPED_RECORDER_STREAM_VARIABLE`]
/// is set and nextest started the process. `tracing-core`'s global default "can only be
/// set once; subsequent attempts to set the global default will fail", so a process that
/// set one first keeps it, and no stream starts.
pub(crate) fn stream_unscoped() {
    if UNSCOPED_TRIED.load(Ordering::Relaxed) || UNSCOPED_TRIED.swap(true, Ordering::AcqRel) {
        return;
    }
    if RECORDER_INSTALLED.load(Ordering::Relaxed)
        || !std::env::args_os().any(|argument| argument == NEXTEST_ARGUMENT)
    {
        return;
    }
    let stream = std::env::var_os(SCOPED_RECORDER_STREAM_VARIABLE).is_some();
    #[cfg(any(test, feature = "fixtures"))]
    let export_enabled = otlp::test_process_export_configured();
    #[cfg(not(any(test, feature = "fixtures")))]
    let export_enabled = false;
    if !stream && !export_enabled {
        return;
    }
    let Ok(filter) = recorder_filter(Some(UNSCOPED_CAPTURE)) else {
        return;
    };
    // No drain reads the queue: a closed queue counts no drop in `log.queue.dropped`.
    let (sink, _drain) = log_capture();
    let flights = Arc::new(FlightTable::default());
    #[cfg(any(test, feature = "fixtures"))]
    let (otlp_layer, test_export) = if export_enabled {
        let runtime = TestOtlpRuntime::start();
        let entered = runtime.handle.enter();
        let (layer, export) = otlp::test_process_layer(filter.clone());
        drop(entered);
        (Some(layer), Some(runtime.with_export(export)))
    } else {
        (None, None)
    };
    let subscriber = crate::capture::registry()
        .with(FlightLayer::new(Arc::clone(&flights)))
        .with(capture_layer(sink, filter))
        .with(unscoped::StopAtRecorder);
    #[cfg(any(test, feature = "fixtures"))]
    let subscriber = subscriber.with(otlp_layer);
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        return;
    }
    #[cfg(any(test, feature = "fixtures"))]
    if let Some(test_export) = test_export {
        test_export.install_meter();
        let _ = UNSCOPED_TEST_EXPORT.set(Mutex::new(Some(test_export)));
    }
    let _ = std::thread::Builder::new()
        .name("rift-unscoped-stream".to_owned())
        .spawn(move || {
            loop {
                std::thread::sleep(UNSCOPED_IN_FLIGHT_INTERVAL);
                let listing = flights.listing(monotonic_now());
                if listing.in_flight > 0 {
                    tracing::info!(
                        target: "rift_tracing::flight",
                        reason = "unscoped_stream",
                        in_flight = listing.in_flight,
                        left_out = listing.left_out,
                        untracked = listing.untracked,
                        operations = %listing,
                        "operations in flight"
                    );
                }
            }
        });
}

/// Shuts down the unscoped test export after a passing test has emitted its records.
#[cfg(test)]
pub(crate) fn shutdown_unscoped_test_export() {
    if let Some(export) = UNSCOPED_TEST_EXPORT.get() {
        let export = { export.lock().unwrap_or_else(PoisonError::into_inner).take() };
        drop(export);
    }
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
    /// Flushes this test's exporter after its thread-local subscriber is restored.
    _test_export: Option<TestOtlpExport>,
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
        let (sink, drain) = log_capture();
        let runtime = TestOtlpRuntime::when_configured();
        let (otlp_layer, export) = if let Some(runtime) = &runtime {
            let _entered = runtime.handle.enter();
            otlp::recorder_layer(filter.clone())
        } else {
            otlp::recorder_layer(filter.clone())
        };
        let meter_provider = export.recorder_meter_provider();
        let metric_reader = export.recorder_metric_reader();
        let metric_exporter = export.recorder_metric_exporter();
        let metric_export = match (runtime.as_ref(), metric_exporter) {
            (Some(runtime), Some(exporter)) => Some(runtime.metric_exporter(exporter)),
            _ => None,
        };
        metrics::install(meter_provider, metric_reader, metric_export);
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
        let test_export = runtime.map(|runtime| runtime.with_export(export));
        let recorder = ScopedRecorder {
            _in_flight: in_flight,
            _default: tracing::subscriber::set_default(subscriber),
            _test_export: test_export,
        };
        Ok((recorder, drain))
    }
}

/// A Tokio runtime held on its owner thread until the scoped recorder shuts its export down.
#[cfg(any(test, feature = "fixtures"))]
enum TestOtlpRequest {
    ExportMetrics {
        exporter: Arc<MetricExporter>,
        metrics: ResourceMetrics,
        deadline: tokio::time::Instant,
        reply: SyncSender<Result<(), ExportShutdownError>>,
    },
    Shutdown {
        export: OtlpExport,
        deadline: tokio::time::Instant,
        reply: SyncSender<Result<(), ExportShutdownError>>,
    },
}

#[cfg(any(test, feature = "fixtures"))]
pub(crate) struct TestOtlpRuntime {
    handle: tokio::runtime::Handle,
    requests: SyncSender<TestOtlpRequest>,
    thread: Option<JoinHandle<()>>,
}

impl TestOtlpRuntime {
    fn when_configured() -> Option<Self> {
        otlp::recorder_export_configured().then(Self::start)
    }

    pub(crate) fn when_configured_without_runtime() -> Option<Self> {
        tokio::runtime::Handle::try_current()
            .is_err()
            .then(Self::when_configured)
            .flatten()
    }

    pub(crate) fn enter(&self) -> tokio::runtime::EnterGuard<'_> {
        self.handle.enter()
    }

    fn start() -> Self {
        let (ready, started) = mpsc::sync_channel(1);
        let (requests, stop) = mpsc::sync_channel::<TestOtlpRequest>(1);
        let thread = thread::Builder::new()
            .name("rift-test-otlp-runtime".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build();
                let runtime = match runtime {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready.send(Err(error.to_string()));
                        return;
                    }
                };
                if ready.send(Ok(runtime.handle().clone())).is_err() {
                    return;
                }
                while let Ok(request) = stop.recv() {
                    match request {
                        TestOtlpRequest::ExportMetrics {
                            exporter,
                            metrics,
                            deadline,
                            reply,
                        } => {
                            let result = runtime.block_on(otlp::export_metric_snapshot(
                                &exporter, &metrics, deadline,
                            ));
                            let _ = reply.send(result);
                        }
                        TestOtlpRequest::Shutdown {
                            export,
                            deadline,
                            reply,
                        } => {
                            let result = runtime.block_on(export.shutdown(deadline));
                            runtime.shutdown_timeout(
                                deadline.saturating_duration_since(tokio::time::Instant::now()),
                            );
                            let _ = reply.send(result);
                            return;
                        }
                    }
                }
                runtime.shutdown_timeout(crate::OTLP_SHUTDOWN_TIMEOUT);
            })
            .expect("the test OTLP runtime thread starts");
        let handle = match started.recv() {
            Ok(Ok(handle)) => handle,
            Ok(Err(error)) => panic!("the test OTLP runtime did not start: {error}"),
            Err(error) => panic!("the test OTLP runtime did not report startup: {error}"),
        };
        Self {
            handle,
            requests,
            thread: Some(thread),
        }
    }

    pub(crate) fn shutdown(mut self, export: OtlpExport) -> Result<(), ExportShutdownError> {
        let deadline = tokio::time::Instant::now() + crate::OTLP_SHUTDOWN_TIMEOUT;
        let (reply, result) = mpsc::sync_channel(1);
        let mut request = TestOtlpRequest::Shutdown {
            export,
            deadline,
            reply,
        };
        let sent = loop {
            match self.requests.try_send(request) {
                Ok(()) => break Ok(()),
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    break Err(ExportShutdownError::Failed(
                        "the test OTLP runtime stopped before export shutdown".to_owned(),
                    ));
                }
                Err(mpsc::TrySendError::Full(held)) => {
                    request = held;
                    if tokio::time::Instant::now() >= deadline {
                        break Err(ExportShutdownError::TimedOut);
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
        };
        let mut outcome = if let Err(error) = sent {
            Err(error)
        } else {
            match result
                .recv_timeout(deadline.saturating_duration_since(tokio::time::Instant::now()))
            {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => Err(ExportShutdownError::TimedOut),
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(ExportShutdownError::Failed(
                    "the test OTLP runtime stopped before reporting export shutdown".to_owned(),
                )),
            }
        };
        if let Some(thread) = self.thread.take() {
            while !thread.is_finished() && tokio::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            if !thread.is_finished() {
                outcome = Err(ExportShutdownError::TimedOut);
            } else if thread.join().is_err() {
                outcome = Err(ExportShutdownError::Failed(
                    "the test OTLP runtime thread panicked".to_owned(),
                ));
            }
        }
        outcome
    }

    fn with_export(self, export: OtlpExport) -> TestOtlpExport {
        TestOtlpExport {
            runtime: Some(self),
            export: Some(export),
        }
    }

    fn metric_exporter(
        &self,
        exporter: Arc<MetricExporter>,
    ) -> crate::recorder::metrics::MetricExport {
        let requests = self.requests.clone();
        Arc::new(move |metrics| {
            let deadline = tokio::time::Instant::now() + otlp::OTLP_EXPORT_TIMEOUT;
            let (reply, response) = mpsc::sync_channel(1);
            let mut request = TestOtlpRequest::ExportMetrics {
                exporter: Arc::clone(&exporter),
                metrics,
                deadline,
                reply,
            };
            loop {
                match requests.try_send(request) {
                    Ok(()) => break,
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        return Err(ExportShutdownError::Failed(
                            "the test OTLP runtime stopped before metric export".to_owned(),
                        ));
                    }
                    Err(mpsc::TrySendError::Full(held)) => {
                        request = held;
                        if tokio::time::Instant::now() >= deadline {
                            return Err(ExportShutdownError::TimedOut);
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }
            match response
                .recv_timeout(deadline.saturating_duration_since(tokio::time::Instant::now()))
            {
                Ok(result) => result,
                Err(mpsc::RecvTimeoutError::Timeout) => Err(ExportShutdownError::TimedOut),
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(ExportShutdownError::Failed(
                    "the test OTLP runtime stopped before reporting metric export".to_owned(),
                )),
            }
        })
    }
}

/// An OTLP export and its runtime, dropped after the recorder restores its prior subscriber.
struct TestOtlpExport {
    runtime: Option<TestOtlpRuntime>,
    export: Option<OtlpExport>,
}

impl std::fmt::Debug for TestOtlpExport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TestOtlpExport")
            .finish_non_exhaustive()
    }
}

impl Drop for TestOtlpExport {
    fn drop(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        if let Some(export) = self.export.take() {
            let result = runtime.shutdown(export);
            if let Err(error) = result
                && !thread::panicking()
            {
                panic!("{error}");
            }
        }
    }
}

impl TestOtlpExport {
    fn install_meter(&self) {
        if let Some(export) = self.export.as_ref() {
            let provider = export.recorder_meter_provider();
            let reader = export.recorder_metric_reader();
            let metric_exporter = export.recorder_metric_exporter();
            let metric_export = match (&self.runtime, metric_exporter) {
                (Some(runtime), Some(exporter)) => Some(runtime.metric_exporter(exporter)),
                _ => None,
            };
            metrics::install(provider, reader, metric_export);
        } else {
            metrics::install(None, None, None);
        }
    }
}

#[cfg(test)]
mod tests;
