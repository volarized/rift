//! The process's subscriber: stderr, the log capture, and the optional OTLP export, each
//! under a filter of its own.
//!
//! Stderr keeps `RUST_LOG` or the default targets, because that stream belongs to whoever
//! started the process. The capture records under the workspace's `[logs] capture` filter,
//! so a workspace can record itself at debug without an operator exporting an environment
//! variable into the process a proxy spawns detached. The export reads `RIFT_OTLP_FILTER`.

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
use crate::flight::{FlightLayer, FlightTable};
use crate::otlp;
use crate::render::LevelColor;
use crate::sampler::{ProcessSampler, SystemProcessReader, TickEvidence};
use crate::stderr::{BoundedStderr, SERVER_STDERR_BYTES_MAX, StderrBound, StderrLines};

/// Default filter keeps dependency diagnostics out of MCP stderr.
pub(crate) const DEFAULT_TRACING_FILTER: &str =
    "rift=info,rift_mcp=info,rift_server=info,rift_index=warn";
/// Default stderr filter: the default targets, with only the stall reports of the table of
/// operations in flight. The table's other records reach the capture under its own filter.
pub(crate) const DEFAULT_STDERR_FILTER: &str = "rift=info,rift_mcp=info,rift_server=info,rift_index=warn,\
                                     rift_tracing::flight=warn";

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
/// event, and the refused builder started no sampler and holds no export.
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
/// Its OTLP export holds nothing unless the `otlp` feature is compiled in and an OTLP
/// endpoint variable names a collector; [`TracingRuntime::shutdown`] runs either way.
#[must_use = "the runtime flushes its export only when shut down"]
pub struct TracingRuntime {
    export: otlp::Export,
    sampler: Option<ProcessSampler>,
}

impl TracingRuntime {
    /// A builder whose subscriber writes stderr unbounded, captures nothing, and samples
    /// no process.
    pub const fn builder() -> TracingRuntimeBuilder {
        TracingRuntimeBuilder {
            capture: None,
            stderr: StderrPolicy::Unbounded,
            stderr_limit: SERVER_STDERR_BYTES_MAX,
            sample_interval: None,
            stall_delay: None,
        }
    }

    /// Stops the process sampler, then flushes buffered spans and shuts the OTLP export
    /// down.
    ///
    /// The caller runs it before either exit path: a normal return drops every other
    /// local first, and `process::exit` past it runs no destructor at all.
    pub fn shutdown(self) {
        if let Some(sampler) = self.sampler {
            sampler.stop();
        }
        self.export.shutdown();
    }
}

/// The settings [`TracingRuntimeBuilder::install`] composes the subscriber from.
#[derive(Debug)]
#[must_use = "a builder installs nothing until `install` runs"]
pub struct TracingRuntimeBuilder {
    capture: Option<String>,
    stderr: StderrPolicy,
    stderr_limit: u64,
    sample_interval: Option<Duration>,
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

    /// Samples the current process every `interval`: its resident and virtual memory, CPU
    /// time and usage, open files, and disk bytes, and the Tokio runtime that installs the
    /// subscriber: its workers, live tasks, global queue depth, worker busy time, and worker
    /// parks. An interval below
    /// [`PROCESS_SAMPLE_INTERVAL_MIN`](crate::PROCESS_SAMPLE_INTERVAL_MIN) samples at that
    /// minimum.
    ///
    /// `interval` is the accepted `[logs] sample_interval` value. Without this call the
    /// runtime reads no process.
    pub const fn sample_interval(mut self, interval: Duration) -> Self {
        self.sample_interval = Some(interval);
        self
    }

    /// Reports, on the process sampler's tick, each operation, lock wait, or held lock
    /// that has stayed open for `delay`: once per entry, as one `WARN` record of the table
    /// of operations in flight with the reason `stall_delay`.
    ///
    /// `delay` is the accepted `[logs] stall_delay` value. Without this call, or without
    /// [`Self::sample_interval`], the runtime reports no stall.
    pub const fn stall_delay(mut self, delay: Duration) -> Self {
        self.stall_delay = Some(delay);
        self
    }

    /// Installs the subscriber as the process's global default, and starts the process
    /// sampler when [`Self::sample_interval`] ran.
    ///
    /// The returned drain exists only when [`Self::capture`] ran; without it the
    /// subscriber has no recording layer and allocates no log queue. The sampler runs on
    /// the calling Tokio runtime; called outside one, the runtime samples nothing and says
    /// so on stderr.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError`] when the process already has a global subscriber, or a
    /// `log` logger: `tracing-subscriber`'s `try_init` refuses a second one. The
    /// installation in place stays untouched, and this builder starts no sampler.
    pub fn install(self) -> Result<(TracingRuntime, Option<LogDrain>), InstallError> {
        let (sink, drain) = match self.capture {
            Some(capture) => {
                let (sink, drain) = log_capture();
                let filter = parsed_filter(&capture)
                    .unwrap_or_else(|_| EnvFilter::new(DEFAULT_TRACING_FILTER));
                (Some(capture_layer(sink, filter)), Some(drain))
            }
            None => (None, None),
        };
        let (otlp_layer, export) = otlp::layer();
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
            export.shutdown();
            return Err(InstallError(error));
        }
        export.install_meter();
        let sampler = self.sample_interval.and_then(|interval| {
            let Ok(handle) = tokio::runtime::Handle::try_current() else {
                eprintln!("rift: warning: no Tokio runtime runs the process sampler");
                return None;
            };
            Some(ProcessSampler::spawn(
                SystemProcessReader::current(),
                interval,
                TickEvidence {
                    runtime: Some(handle.metrics()),
                    flights: Some(Arc::clone(&flights)),
                    stall_delay: self.stall_delay,
                },
            ))
        });
        Ok((TracingRuntime { export, sampler }, drain))
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
