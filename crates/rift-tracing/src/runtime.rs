//! The process's subscriber: stderr, the log capture, and the optional OTLP export, each
//! under a filter of its own.
//!
//! Stderr keeps `RUST_LOG` or the default targets, because that stream belongs to whoever
//! started the process. The capture records under the workspace's `[logs] capture` filter,
//! so a workspace can record itself at debug without an operator exporting an environment
//! variable into the process a proxy spawns detached. The span export reads
//! `RIFT_OTLP_FILTER`; the log record export runs under the capture's filter.

use std::fmt;
use std::io::IsTerminal as _;
use std::sync::Arc;
use std::time::Duration;

use tracing::Subscriber;
use tracing::subscriber::Interest;
use tracing_subscriber::filter::{DynFilterFn, FilterExt as _, LevelFilter, ParseError};
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::layer::{Filter, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::{SubscriberInitExt as _, TryInitError};
use tracing_subscriber::{EnvFilter, Layer};

use crate::capture::{LogSink, log_capture};
use crate::drain::LogDrain;
use crate::flight::{FlightLayer, FlightTable, StallReport, observe_active};
use crate::metrics::ObservationGuard;
use crate::otlp::{self, OtlpExport};
#[cfg(any(test, feature = "fixtures"))]
use crate::recorder::TestOtlpRuntime;
use crate::render::LevelColor;
use crate::sampler::{SystemProcessReader, observe_process, observe_runtime};
use crate::stderr::{BoundedStderr, SERVER_STDERR_BYTES_MAX, StderrBound, StderrLines};

/// Default filter keeps dependency diagnostics out of MCP stderr.
pub(crate) const DEFAULT_TRACING_FILTER: &str =
    "rift=info,rift_mcp=info,rift_server=info,rift_index=warn,rift_tracing::runtime=info";
/// Default stderr filter: the default targets, with only the stall reports of the table of
/// operations in flight. The table's other records reach the capture under its own filter.
pub(crate) const DEFAULT_STDERR_FILTER: &str = "rift=info,rift_mcp=info,rift_server=info,rift_index=warn,\
                                     rift_tracing::runtime=warn,rift_tracing::flight=warn";

/// How much the process may write to its standard error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StderrPolicy {
    /// Everything the filter admits: the stream belongs to whoever reads it.
    Unbounded,
    /// At most the bytes [`TracingRuntimeBuilder::stderr_limit`] sets,
    /// [`SERVER_STDERR_BYTES_MAX`](crate::SERVER_STDERR_BYTES_MAX) without it: the stream is
    /// the file `rift server start` handed its detached server.
    Bounded,
}

impl StderrPolicy {
    /// The policy for this process, given whether it serves a workspace.
    ///
    /// A server whose stderr is not a terminal is writing into a file or a pipe that
    /// outlives every reader, so it is bounded; every other command, and a server an
    /// operator watches in a terminal, writes freely.
    #[must_use]
    pub fn of_process(serves: bool) -> Self {
        Self::of(serves, std::io::stderr().is_terminal())
    }

    const fn of(serves: bool, terminal: bool) -> Self {
        if serves && !terminal {
            Self::Bounded
        } else {
            Self::Unbounded
        }
    }
}

/// A `[logs] capture` value `tracing` cannot parse as a filter.
#[derive(Debug)]
pub struct LogFilterError(ParseError);

impl fmt::Display for LogFilterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for LogFilterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Checks `filter` in the `RUST_LOG` spelling the capture filter takes.
///
/// # Errors
///
/// Returns [`LogFilterError`] when `filter` is not a list of `target=level` directives
/// `tracing` accepts.
pub fn validate_log_filter(filter: &str) -> Result<(), LogFilterError> {
    parsed_filter(filter).map(|_| ())
}

/// `filter` parsed in the `RUST_LOG` spelling.
pub(crate) fn parsed_filter(filter: &str) -> Result<EnvFilter, LogFilterError> {
    EnvFilter::try_new(filter).map_err(LogFilterError)
}

/// The filter the capture records under: `capture`, the accepted `[logs] capture` value, or
/// the default targets when it is absent or `tracing` cannot parse it.
///
/// The OTLP log record export runs under the same filter, so it carries the records the
/// store keeps; a process without a capture exports under the default targets.
fn capture_filter(capture: Option<&str>) -> EnvFilter {
    capture
        .and_then(|capture| parsed_filter(capture).ok())
        .unwrap_or_else(|| EnvFilter::new(DEFAULT_TRACING_FILTER))
}

/// The capture layer: `sink` under `filter`, asked at every span and event.
pub(crate) fn capture_layer<S>(sink: LogSink, filter: EnvFilter) -> impl Layer<S>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    sink.with_filter(reevaluated(filter))
}

/// A [`TracingRuntimeBuilder::install`] that found the process's global subscriber, or a
/// `log` logger, already installed.
///
/// The installation already in place stays as it was: it keeps receiving every span and
/// event, and the refused builder started no stall report and holds no export.
#[derive(Debug)]
pub struct InstallError(TryInitError);

impl fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "tracing is already installed in this process: {}",
            self.0
        )
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// The installed subscriber's handle, held until the process stops tracing.
///
/// Its OTLP export holds nothing unless an OTLP endpoint variable names a collector;
/// [`TracingRuntime::shutdown`] runs either way.
#[must_use = "the runtime flushes its export only when shut down"]
pub struct TracingRuntime {
    export: OtlpExport,
    stall: Option<StallReport>,
    /// Keeps the table of operations in flight reported in `operation.active`.
    _in_flight: Option<ObservationGuard>,
    #[cfg(any(test, feature = "fixtures"))]
    test_otlp_runtime: Option<TestOtlpRuntime>,
}

/// How long [`TracingRuntime::shutdown`] waits for the OTLP export's final flush and
/// shutdown before the process leaves without them.
pub const OTLP_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
/// Time inside [`OTLP_SHUTDOWN_TIMEOUT`] left for the final log export.
const OTLP_LOG_SHUTDOWN_RESERVE: Duration = Duration::from_millis(50);

impl TracingRuntime {
    /// A builder whose subscriber writes stderr unbounded, captures nothing, and reports
    /// no stall.
    pub const fn builder() -> TracingRuntimeBuilder {
        TracingRuntimeBuilder {
            capture: None,
            stderr: StderrPolicy::Unbounded,
            stderr_limit: SERVER_STDERR_BYTES_MAX,
            stall_delay: None,
        }
    }

    /// The process's OTLP export, for a stop that shuts it down inside its own budget; a
    /// later [`Self::shutdown`] then finds it shut down.
    #[must_use]
    pub fn export(&self) -> OtlpExport {
        self.export.clone()
    }

    /// Stops the stall report and joins its task, then flushes buffered spans, log records,
    /// and metric points and shuts the OTLP export down, waiting at most
    /// [`OTLP_SHUTDOWN_TIMEOUT`].
    ///
    /// The caller runs it before either exit path: a normal return drops every other
    /// local first, and `process::exit` past it runs no destructor at all. A traces and
    /// metrics failure is recorded at `WARN` while the log provider remains open. The log
    /// provider shutdown result takes precedence when both phases fail. Neither changes
    /// the process's success status.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ExportShutdownError::TimedOut`] when a phase exceeds its bound, or
    /// [`crate::ExportShutdownError::Failed`] when a provider reports an export or
    /// shutdown failure.
    pub async fn shutdown(self) -> Result<(), crate::ExportShutdownError> {
        let Self {
            export,
            stall,
            _in_flight,
            #[cfg(any(test, feature = "fixtures"))]
            test_otlp_runtime,
        } = self;
        if let Some(stall) = stall {
            stall.stop().await;
        }
        let deadline = tokio::time::Instant::now() + OTLP_SHUTDOWN_TIMEOUT;
        #[cfg(any(test, feature = "fixtures"))]
        if let Some(runtime) = test_otlp_runtime {
            return runtime.shutdown(export);
        }

        let providers_deadline = deadline - OTLP_LOG_SHUTDOWN_RESERVE;
        let started = tokio::time::Instant::now();
        let providers = export.shutdown_traces_and_metrics(providers_deadline).await;
        record_shutdown_result(
            &providers,
            "traces and metrics",
            started,
            providers_deadline,
        );

        let logs = export.shutdown_logs(deadline).await;
        logs.and(providers)
    }
}

/// The settings [`TracingRuntimeBuilder::install`] composes the subscriber from.
#[derive(Debug)]
#[must_use = "a builder installs nothing until `install` runs"]
pub struct TracingRuntimeBuilder {
    capture: Option<String>,
    stderr: StderrPolicy,
    stderr_limit: u64,
    stall_delay: Option<Duration>,
}

impl TracingRuntimeBuilder {
    /// Records what `filter` admits into the log drain [`Self::install`] returns.
    ///
    /// `filter` is the accepted `[logs] capture` value; a value `tracing` cannot parse
    /// records under the default targets instead.
    pub fn capture(mut self, filter: &str) -> Self {
        self.capture = Some(filter.to_owned());
        self
    }

    /// Bounds or frees the process's standard error.
    pub const fn stderr(mut self, policy: StderrPolicy) -> Self {
        self.stderr = policy;
        self
    }

    /// Bounds a [`StderrPolicy::Bounded`] standard error at `bytes`: the writer passes that
    /// many, prints one notice, and discards the rest, counting the discarded bytes in
    /// `log.stderr.discarded`. The log drain's stop records the count.
    ///
    /// `bytes` is the accepted `[logs] stderr_limit` value. Without this call the bound is
    /// [`SERVER_STDERR_BYTES_MAX`](crate::SERVER_STDERR_BYTES_MAX); under
    /// [`StderrPolicy::Unbounded`] it bounds nothing.
    pub const fn stderr_limit(mut self, bytes: u64) -> Self {
        self.stderr_limit = bytes;
        self
    }

    /// Reports each operation, lock wait, or held lock that has stayed open for `delay`:
    /// once per entry, as one `WARN` record of the table of operations in flight with the
    /// reason `stall_delay`. The report's task reads the table every quarter of `delay`,
    /// between a quarter second and five seconds, so a report comes at most that tick late.
    ///
    /// `delay` is the accepted `[logs] stall_delay` value. Without this call the runtime
    /// reports no stall.
    pub const fn stall_delay(mut self, delay: Duration) -> Self {
        self.stall_delay = Some(delay);
        self
    }

    /// Installs the subscriber as the process's global default, registers the process and
    /// Tokio runtime readings when an OTLP endpoint installed a meter, and starts the stall
    /// report when [`Self::stall_delay`] ran.
    ///
    /// The returned drain exists only when [`Self::capture`] ran; without it the
    /// subscriber has no recording layer and allocates no log queue. The stall report runs
    /// on the calling Tokio runtime, and the runtime readings read it; called outside one,
    /// the runtime reports no stall, reads no runtime, and records a warning.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError`] when the process already has a global subscriber, or a
    /// `log` logger: `tracing-subscriber`'s `try_init` refuses a second one. The
    /// installation in place stays untouched, and this builder starts no stall report.
    pub fn install(self) -> Result<(TracingRuntime, Option<LogDrain>), InstallError> {
        let (sink, drain) = match self.capture.as_deref() {
            Some(capture) => {
                let (sink, drain) = log_capture();
                let filter = capture_filter(Some(capture));
                (Some(capture_layer(sink, filter)), Some(drain))
            }
            None => (None, None),
        };
        #[cfg(any(test, feature = "fixtures"))]
        let test_otlp_runtime = TestOtlpRuntime::when_configured_without_runtime();
        #[cfg(any(test, feature = "fixtures"))]
        let (otlp_layer, export) = if let Some(runtime) = &test_otlp_runtime {
            let _entered = runtime.enter();
            otlp::layer(capture_filter(self.capture.as_deref()))
        } else {
            otlp::layer(capture_filter(self.capture.as_deref()))
        };
        #[cfg(not(any(test, feature = "fixtures")))]
        let (otlp_layer, export) = otlp::layer(capture_filter(self.capture.as_deref()));
        let (writer, drain) = match self.stderr {
            StderrPolicy::Unbounded => (BoxMakeWriter::new(std::io::stderr), drain),
            StderrPolicy::Bounded => {
                let bound = Arc::new(StderrBound::new(self.stderr_limit));
                let drain = drain.map(|drain| drain.with_stderr(Arc::clone(&bound)));
                let writer = BoundedStderr::new(bound);
                (BoxMakeWriter::new(writer), drain)
            }
        };
        // Escape codes color a terminal. A pipe or a file hands them to its reader as bytes:
        // `rift mcp` keeps a spawned server's first startup lines verbatim, and the codes
        // nearly double each line.
        let color = if std::io::stderr().is_terminal() {
            LevelColor::Ansi
        } else {
            LevelColor::Plain
        };
        let stderr_layer = StderrLines::new(writer, color);
        let flights = Arc::new(FlightTable::default());
        let installed = crate::capture::registry()
            .with(FlightLayer::new(Arc::clone(&flights)))
            .with(
                stderr_layer.with_filter(stderr_filter(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new(DEFAULT_STDERR_FILTER)),
                )),
            )
            .with(sink)
            .with(otlp_layer)
            .try_init();
        if let Err(error) = installed {
            #[cfg(any(test, feature = "fixtures"))]
            if let Some(runtime) = test_otlp_runtime {
                let _ = runtime.shutdown(export);
                return Err(InstallError(error));
            }
            // Dropping the providers blocks on their shutdown, which waits for export tasks
            // this runtime drives; a thread of its own drops them instead.
            let _ = std::thread::Builder::new()
                .name("rift-otlp-shutdown".to_owned())
                .spawn(move || drop(export));
            return Err(InstallError(error));
        }
        export.install_meter();
        let in_flight = observe_active(&flights);
        let _process = observe_process(SystemProcessReader::current());
        let runtime = tokio::runtime::Handle::try_current();
        if let Ok(handle) = &runtime {
            let _runtime = observe_runtime(&handle.metrics());
        }
        let stall = self.stall_delay.and_then(|delay| {
            if runtime.is_err() {
                crate::warn!(
                    target: "rift",
                    "no Tokio runtime runs the stall report"
                );
                return None;
            }
            Some(StallReport::spawn(Arc::clone(&flights), delay))
        });
        Ok((
            TracingRuntime {
                export,
                stall,
                _in_flight: in_flight,
                #[cfg(any(test, feature = "fixtures"))]
                test_otlp_runtime,
            },
            drain,
        ))
    }
}

fn record_shutdown_result(
    result: &Result<(), otlp::ExportShutdownError>,
    phase: &'static str,
    started: tokio::time::Instant,
    deadline: tokio::time::Instant,
) {
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    match result {
        Ok(()) => crate::info!(
            stage = "otlp export",
            phase,
            outcome = "ok",
            elapsed_ms,
            ?remaining,
            "stop stage ended"
        ),
        Err(otlp::ExportShutdownError::TimedOut) => crate::warn!(
            stage = "otlp export",
            phase,
            outcome = "timeout",
            elapsed_ms,
            ?remaining,
            "stop stage ended"
        ),
        Err(error @ otlp::ExportShutdownError::Failed(_)) => crate::warn!(
            stage = "otlp export",
            phase,
            outcome = "error",
            elapsed_ms,
            ?remaining,
            %error,
            "stop stage ended"
        ),
    }
}

/// The stderr filter: `operator` plus the OTLP export's own reports.
///
/// `operator` is `RUST_LOG` or the default targets, and rarely names the OpenTelemetry SDK's
/// target, through which the export reports what it drops; `otlp::sdk_reports` rides beside
/// it so those reports reach stderr either way.
pub(crate) fn stderr_filter<S>(operator: EnvFilter) -> impl Filter<S> {
    reevaluated(operator.or(otlp::sdk_reports()))
}

/// Wraps a per-layer filter so the subscriber asks it at every span and event.
///
/// `tracing-subscriber` hands each per-layer filter's answer to the registry through one
/// thread-local state that only an `enabled` pass writes, and a callsite whose interest is
/// cached as `always` opens its span without such a pass. `tracing::event_enabled!` runs a
/// pass and dispatches nothing - `toasty` asks it about a `toasty::query` warning before
/// every statement - so without this wrapper the next such span on that thread inherits the
/// probe's answers, and each layer whose filter refused the probe loses the span: the log
/// store does not record it, and the OTLP export does not export it.
///
/// The `DynFilterFn` beside `filter` enables everything and answers `sometimes` for every
/// callsite `filter` does not refuse, so no callsite's interest is cached as `always`. Its
/// `TRACE` hint leaves `filter`'s own level hint in force.
pub(crate) fn reevaluated<S>(filter: impl Filter<S>) -> impl Filter<S> {
    let every_time = DynFilterFn::new(|_, _| true)
        .with_callsite_filter(|_| Interest::sometimes())
        .with_max_level_hint(LevelFilter::TRACE);
    filter.and(every_time)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing::span::{Attributes, Id};
    use tracing_subscriber::layer::{Context, SubscriberExt as _};
    use tracing_subscriber::{EnvFilter, Layer};

    use super::{StderrPolicy, validate_log_filter};

    #[test]
    fn only_a_server_off_a_terminal_bounds_its_stderr() {
        assert_eq!(StderrPolicy::of(true, false), StderrPolicy::Bounded);
        assert_eq!(StderrPolicy::of(true, true), StderrPolicy::Unbounded);
        assert_eq!(StderrPolicy::of(false, false), StderrPolicy::Unbounded);
        assert_eq!(StderrPolicy::of(false, true), StderrPolicy::Unbounded);
    }

    #[test]
    fn a_log_filter_is_checked_in_the_rust_log_spelling() {
        assert!(validate_log_filter("rift=info,rift_mcp=debug").is_ok());
        let refused = validate_log_filter("rift=loud").expect_err("an unknown level is refused");
        assert_eq!(
            refused.to_string(),
            EnvFilter::try_new("rift=loud")
                .expect_err("tracing refuses the same value")
                .to_string(),
            "the refusal carries tracing's own words"
        );
    }

    /// Spans the filter test opens; enough that one lost span shows as a count mismatch.
    const OPENED_SPANS: usize = 32;

    /// Every span name one layer saw open, in order.
    #[derive(Clone, Default)]
    struct OpenedSpans {
        names: Arc<Mutex<Vec<&'static str>>>,
    }

    impl OpenedSpans {
        fn count(&self, name: &str) -> usize {
            self.names
                .lock()
                .expect("the opened span names are not poisoned")
                .iter()
                .filter(|opened| **opened == name)
                .count()
        }
    }

    impl<S: tracing::Subscriber> Layer<S> for OpenedSpans {
        fn on_new_span(&self, attributes: &Attributes<'_>, _id: &Id, _context: Context<'_, S>) {
            self.names
                .lock()
                .expect("the opened span names are not poisoned")
                .push(attributes.metadata().name());
        }
    }

    /// `toasty` asks `tracing::event_enabled!` about a `toasty::query` warning before every
    /// statement. A stderr filter with a bare `warn` default, such as `RUST_LOG=warn,rift=info`,
    /// enables that probe while the log store's capture filter refuses it, and the capture
    /// must still see every span that follows on the thread.
    #[test]
    fn a_reevaluated_filter_sees_every_span_after_a_probe_it_refuses() {
        let capture = OpenedSpans::default();
        let subscriber = tracing_subscriber::registry()
            .with(
                OpenedSpans::default()
                    .with_filter(super::reevaluated(EnvFilter::new("warn,rift=info"))),
            )
            .with(
                capture
                    .clone()
                    .with_filter(super::reevaluated(EnvFilter::new("rift=info"))),
            );
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..OPENED_SPANS {
                let _ = tracing::event_enabled!(target: "toasty::query", tracing::Level::WARN);
                crate::traced!(component = "search", operation = "search.request", {});
            }
        });
        assert_eq!(capture.count("search.request"), OPENED_SPANS);
    }
}
