//! The OTLP export of Rift's `tracing` spans and metrics.
//!
//! Every build carries the export, and a process exports nothing until an operator sets an
//! OTLP endpoint variable - for the in-memory collector `just trace-collector` runs, or any
//! other OTLP/HTTP receiver. Spans export when `OTEL_EXPORTER_OTLP_ENDPOINT` is set;
//! metrics when it or `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` is.
//!
//! Every exported span and metric carries the resource attributes `service.name` (`rift`),
//! `service.version` (the workspace version), `service.instance.id` (random per process),
//! and `process.pid`, so a collector that receives from several servers tells them apart.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{MetricExporter, Protocol, SpanExporter, WithExportConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkError;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
use opentelemetry_sdk::runtime;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::{BatchConfig, BatchConfigBuilder, SdkTracerProvider};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::registry::LookupSpan;

/// The `service.name` resource attribute every exported span and metric carries.
const SERVICE_NAME: &str = "rift";
/// The `service.version` resource attribute: the workspace version every Rift crate takes,
/// the one `rift --version` starts with.
const SERVICE_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Overrides the export layer's own filter; unset, [`DEFAULT_OTLP_FILTER`] applies.
const RIFT_OTLP_FILTER_VAR: &str = "RIFT_OTLP_FILTER";
/// Keeps Rift's own crates - the ones `traced!` instruments - at info; a dependency's own
/// spans stay out unless the operator names it.
const DEFAULT_OTLP_FILTER: &str =
    "rift=info,rift_mcp=info,rift_server=info,rift_index=info,rift_analysis=info";

/// Most time one export of metrics or of a span batch takes before the SDK gives it up,
/// when `OTEL_METRIC_EXPORT_TIMEOUT` or `OTEL_BSP_EXPORT_TIMEOUT` sets none: the SDK's own
/// default is 30 s. A given-up export is reported on stderr and its points sent again by
/// the next cumulative export.
pub(crate) const OTLP_EXPORT_TIMEOUT: Duration = Duration::from_secs(2);
/// The variable the SDK reads the metric export timeout from.
const METRIC_EXPORT_TIMEOUT_VAR: &str = "OTEL_METRIC_EXPORT_TIMEOUT";
/// The variable the SDK reads the span export timeout from.
const SPAN_EXPORT_TIMEOUT_VAR: &str = "OTEL_BSP_EXPORT_TIMEOUT";

/// The target the OpenTelemetry SDK's own reports carry.
pub(crate) const SDK_TARGET: &str = "opentelemetry_sdk";

/// The OpenTelemetry SDK's own warnings and errors, which stderr carries whatever
/// `RUST_LOG` names.
///
/// The batch processor queues at most `OTEL_BSP_MAX_QUEUE_SIZE` ended spans, 2048 when the
/// variable is unset, and drops a span it cannot queue. It reports the first drop and the
/// dropped total at shutdown as warnings, and a failed export as an error, so a full queue
/// or an unreachable collector reaches the operator instead of thinning the trace unseen.
pub(crate) fn sdk_reports() -> Targets {
    Targets::new().with_target(SDK_TARGET, LevelFilter::WARN)
}

/// The installed exporter's tracer and meter providers, shut down at most once.
struct Providers {
    tracer: Option<SdkTracerProvider>,
    meters: Option<SdkMeterProvider>,
}

/// The process's OTLP export: the installed tracer and meter providers, held so the process
/// flushes and shuts them down before it exits.
///
/// Holds nothing when no collector endpoint was configured; [`Self::shutdown`] then
/// answers at once, as it does for [`OtlpExport::default`]. Clones share the providers, and
/// the first shutdown takes them, so a later one answers at once.
#[derive(Clone, Default)]
pub struct OtlpExport {
    providers: Arc<Mutex<Option<Providers>>>,
}

impl std::fmt::Debug for OtlpExport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = self
            .providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|providers| (providers.tracer.is_some(), providers.meters.is_some()));
        formatter
            .debug_struct("OtlpExport")
            .field("tracer_and_meters", &held)
            .finish()
    }
}

/// Why an export shutdown did not end cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportShutdownError {
    /// The deadline passed first. The shutdown keeps running on its own thread until the
    /// process exits, and the points and spans it had not sent are lost.
    TimedOut,
    /// The final export or the shutdown failed, with the SDK's words.
    Failed(String),
}

impl std::fmt::Display for ExportShutdownError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut => formatter.write_str("the otlp export shutdown passed its deadline"),
            Self::Failed(reason) => write!(formatter, "the otlp export shutdown failed: {reason}"),
        }
    }
}

impl std::error::Error for ExportShutdownError {}

impl OtlpExport {
    /// An export that holds `tracer` and `meters`.
    fn holding(tracer: Option<SdkTracerProvider>, meters: Option<SdkMeterProvider>) -> Self {
        Self {
            providers: Arc::new(Mutex::new(Some(Providers { tracer, meters }))),
        }
    }

    /// Makes the meter provider's meter the one every instrument records into, when one
    /// exports. The runtime calls it once its subscriber is installed.
    pub(crate) fn install_meter(&self) {
        let providers = self
            .providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(meters) = providers.as_ref().and_then(|held| held.meters.as_ref()) {
            crate::metrics::install_meter(meters.meter_with_scope(crate::metrics::scope()));
        }
    }

    /// Flushes buffered spans and the final metric points, and shuts both providers down,
    /// by `deadline`.
    ///
    /// The SDK's shutdown calls block and the async-runtime reader and batch processor
    /// ignore the timeout they are handed: each waits for its worker task, which runs one
    /// more export bounded by the export timeout alone. Each shutdown therefore runs on a
    /// thread of its own, and this call stops waiting at `deadline` whatever the export
    /// timeouts are set to. A thread past `deadline` keeps running until the process exits;
    /// the runtime that drives the export is never asked to wait for it.
    ///
    /// # Errors
    ///
    /// Returns [`ExportShutdownError::TimedOut`] when `deadline` passed first, and
    /// [`ExportShutdownError::Failed`] when a provider reported a failed final export or
    /// shutdown. Neither is the process's failure: the export carries diagnostics only.
    pub async fn shutdown(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<(), ExportShutdownError> {
        let taken = self
            .providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(Providers { tracer, meters }) = taken else {
            return Ok(());
        };
        let tracer = tracer.map(|tracer| shut_down_on_thread(move || tracer.shutdown()));
        let meters = meters.map(|meters| shut_down_on_thread(move || meters.shutdown()));
        let waited = tokio::time::timeout_at(deadline, async move {
            let mut failures = Vec::new();
            for answer in [tracer, meters].into_iter().flatten() {
                match answer.await {
                    Ok(Ok(()) | Err(OTelSdkError::AlreadyShutdown)) => {}
                    Ok(Err(error)) => failures.push(error.to_string()),
                    Err(_) => failures.push("the shutdown thread did not start".to_owned()),
                }
            }
            failures
        })
        .await;
        match waited {
            Err(_) => Err(ExportShutdownError::TimedOut),
            Ok(failures) if failures.is_empty() => Ok(()),
            Ok(failures) => Err(ExportShutdownError::Failed(failures.join("; "))),
        }
    }
}

/// Runs `shutdown` on a thread of its own, and answers its result when it returns.
fn shut_down_on_thread(
    shutdown: impl FnOnce() -> Result<(), OTelSdkError> + Send + 'static,
) -> tokio::sync::oneshot::Receiver<Result<(), OTelSdkError>> {
    let (sent, answer) = tokio::sync::oneshot::channel();
    // A thread that cannot start drops `sent`, and the answer reports it.
    let _ = std::thread::Builder::new()
        .name("rift-otlp-shutdown".to_owned())
        .spawn(move || {
            let _ = sent.send(shutdown());
        });
    answer
}

/// The variables that name where metrics export: the metrics endpoint, used as it is, or
/// the base endpoint, which the exporter extends with `/v1/metrics`.
const METRIC_ENDPOINT_VARS: [&str; 2] = [
    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
];

/// Whether the process sets `variable`.
fn configured(variable: &str) -> bool {
    std::env::var_os(variable).is_some()
}

/// The resource every exported span and metric carries: `service.name`, `service.version`,
/// a random `service.instance.id` in the UUID version 4 form, and `process.pid`.
fn resource() -> Resource {
    let mut builder = Resource::builder()
        .with_service_name(SERVICE_NAME)
        .with_attribute(KeyValue::new("service.version", SERVICE_VERSION));
    if let Some(instance) = instance_id() {
        builder = builder.with_attribute(KeyValue::new("service.instance.id", instance));
    }
    builder
        .with_attribute(KeyValue::new("process.pid", i64::from(std::process::id())))
        .build()
}

/// A random identifier in the UUID version 4 form, `None` when the platform's random
/// source fails.
fn instance_id() -> Option<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes
        .iter()
        .fold(String::with_capacity(32), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// Installs an OTLP/HTTP export layer when `OTEL_EXPORTER_OTLP_ENDPOINT` names a
/// collector, `None` otherwise, beside a meter provider when a metric endpoint variable
/// names one.
///
/// Generic in the subscriber `S` because `tracing_subscriber::registry().with(a).with(b)`
/// changes the concrete subscriber type at every `.with()` call; a layer boxed as
/// `dyn Layer<Registry>` only satisfies the first one in the chain. Returning `impl
/// Layer<S>` lets this layer adapt to wherever [`TracingRuntimeBuilder::install`] appends it
/// instead.
///
/// [`TracingRuntimeBuilder::install`]: crate::TracingRuntimeBuilder::install
///
/// The exporter posts protobuf-encoded OTLP over the async `reqwest` client Rift already
/// depends on, batched by a Tokio-driven [`BatchSpanProcessor`]: `opentelemetry_sdk`'s
/// default batch processor exports on a dedicated `std::thread` through
/// `futures_executor::block_on`, which has no Tokio reactor to poll an async HTTP client
/// on, and Rift never uses `reqwest::blocking`. Checking the endpoint variables before
/// building anything keeps the export from silently dialing OTLP's default
/// `http://localhost:4318`.
pub(crate) fn layer<S>() -> (Option<impl Layer<S> + Send + Sync>, OtlpExport)
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
{
    let resource = resource();
    let meters = if METRIC_ENDPOINT_VARS
        .iter()
        .any(|variable| configured(variable))
    {
        match MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()
        {
            Ok(exporter) => Some(meter_provider(exporter, resource.clone())),
            Err(error) => {
                eprintln!("rift: warning: otlp metric exporter did not build: {error}");
                None
            }
        }
    } else {
        None
    };
    if !configured("OTEL_EXPORTER_OTLP_ENDPOINT") {
        return (None, OtlpExport::holding(None, meters));
    }
    let exporter = match SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()
    {
        Ok(exporter) => exporter,
        Err(error) => {
            eprintln!("rift: warning: otlp exporter did not build: {error}");
            return (None, OtlpExport::holding(None, meters));
        }
    };
    let provider = tracer_provider(exporter, batch_config(), resource);
    let filter = std::env::var(RIFT_OTLP_FILTER_VAR)
        .ok()
        .and_then(|value| tracing_subscriber::EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new(DEFAULT_OTLP_FILTER));
    (
        Some(export_layer(&provider, filter)),
        OtlpExport::holding(Some(provider), meters),
    )
}

/// The batch processor's settings: the SDK's, read from the `OTEL_BSP_*` variables, with
/// [`OTLP_EXPORT_TIMEOUT`] when `OTEL_BSP_EXPORT_TIMEOUT` sets no export timeout.
fn batch_config() -> BatchConfig {
    let builder = BatchConfigBuilder::default();
    if configured(SPAN_EXPORT_TIMEOUT_VAR) {
        builder.build()
    } else {
        builder.with_max_export_timeout(OTLP_EXPORT_TIMEOUT).build()
    }
}

/// The meter provider that exports what every instrument records into `exporter`.
///
/// Its reader exports on the Tokio runtime, at `OTEL_METRIC_EXPORT_INTERVAL` or the SDK's
/// 60 s default, so the OTLP exporter posts over the same async `reqwest` client the span
/// batches use. One export takes at most `OTEL_METRIC_EXPORT_TIMEOUT`, or
/// [`OTLP_EXPORT_TIMEOUT`] when the variable is unset. Must be called inside a Tokio
/// runtime.
fn meter_provider<E>(exporter: E, resource: Resource) -> SdkMeterProvider
where
    E: opentelemetry_sdk::metrics::exporter::PushMetricExporter,
{
    let reader = PeriodicReader::builder(exporter, runtime::Tokio);
    let reader = if configured(METRIC_EXPORT_TIMEOUT_VAR) {
        reader
    } else {
        reader.with_timeout(OTLP_EXPORT_TIMEOUT)
    };
    SdkMeterProvider::builder()
        .with_resource(resource)
        .with_reader(reader.build())
        .with_view(crate::metrics::cardinality_view(
            crate::metrics::CARDINALITY_LIMIT,
        ))
        .build()
}

/// The tracer provider that batches every ended span into `exporter` under `batch`.
///
/// Must be called inside a Tokio runtime: the batch processor spawns its export task
/// there.
fn tracer_provider<E>(exporter: E, batch: BatchConfig, resource: Resource) -> SdkTracerProvider
where
    E: opentelemetry_sdk::trace::SpanExporter + 'static,
{
    let processor = BatchSpanProcessor::builder(exporter, runtime::Tokio)
        .with_batch_config(batch)
        .build();
    SdkTracerProvider::builder()
        .with_resource(resource)
        .with_span_processor(processor)
        .build()
}

/// The layer that hands every span `filter` enables to `provider`.
///
/// `filter` is reevaluated at every span, so a span the export filter enables reaches
/// `provider` whatever pass another layer's filter ran last on that thread.
fn export_layer<S>(
    provider: &SdkTracerProvider,
    filter: tracing_subscriber::EnvFilter,
) -> impl Layer<S> + Send + Sync + use<S>
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
{
    tracing_opentelemetry::layer()
        .with_tracer(provider.tracer(SERVICE_NAME))
        .with_filter(crate::runtime::reevaluated(filter))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{BatchConfig, BatchConfigBuilder, SpanData, SpanExporter};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, SubscriberExt as _};
    use tracing_subscriber::{EnvFilter, Layer};

    use super::{
        ExportShutdownError, OtlpExport, export_layer, meter_provider, resource, tracer_provider,
    };

    /// Spans each test ends; enough that one lost span shows as a count mismatch.
    const SPANS: usize = 32;

    /// Every span name one exporter received, in export order.
    #[derive(Clone, Debug, Default)]
    struct RecordingExporter {
        names: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingExporter {
        fn count(&self, name: &str) -> usize {
            self.names
                .lock()
                .expect("the exporter's names are not poisoned")
                .iter()
                .filter(|exported| exported.as_str() == name)
                .count()
        }
    }

    impl SpanExporter for RecordingExporter {
        fn export(
            &self,
            batch: Vec<SpanData>,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            self.names
                .lock()
                .expect("the exporter's names are not poisoned")
                .extend(batch.into_iter().map(|span| span.name.into_owned()));
            std::future::ready(Ok(()))
        }
    }

    /// An exporter whose export never finishes, so the batch processor's queue fills.
    #[derive(Debug)]
    struct StalledExporter;

    impl SpanExporter for StalledExporter {
        fn export(
            &self,
            _batch: Vec<SpanData>,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            std::future::pending()
        }
    }

    /// One event a layer received: its name and its `dropped_spans` field, if any.
    #[derive(Clone, Copy, Debug)]
    struct Report {
        name: &'static str,
        dropped_spans: Option<u64>,
    }

    /// Every event one layer received, in order.
    #[derive(Clone, Default)]
    struct Reports {
        events: Arc<Mutex<Vec<Report>>>,
    }

    impl Reports {
        fn count(&self, name: &str) -> usize {
            self.events
                .lock()
                .expect("the reports are not poisoned")
                .iter()
                .filter(|report| report.name == name)
                .count()
        }

        fn dropped_spans(&self) -> Option<u64> {
            self.events
                .lock()
                .expect("the reports are not poisoned")
                .iter()
                .find_map(|report| report.dropped_spans)
        }
    }

    /// Reads the `dropped_spans` field of one event.
    #[derive(Default)]
    struct DroppedSpans(Option<u64>);

    impl Visit for DroppedSpans {
        fn record_u64(&mut self, field: &Field, value: u64) {
            if field.name() == "dropped_spans" {
                self.0 = Some(value);
            }
        }

        fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
    }

    impl<S: tracing::Subscriber> Layer<S> for Reports {
        fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
            let mut dropped = DroppedSpans::default();
            event.record(&mut dropped);
            self.events
                .lock()
                .expect("the reports are not poisoned")
                .push(Report {
                    name: event.metadata().name(),
                    dropped_spans: dropped.0,
                });
        }
    }

    /// Every metric name and unit one metric exporter received, in export order.
    #[derive(Clone, Debug, Default)]
    struct RecordingMetricExporter {
        metrics: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl opentelemetry_sdk::metrics::exporter::PushMetricExporter for RecordingMetricExporter {
        fn export(
            &self,
            metrics: &opentelemetry_sdk::metrics::data::ResourceMetrics,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            let mut received = self
                .metrics
                .lock()
                .expect("the exporter's metrics are not poisoned");
            for scope in metrics.scope_metrics() {
                for metric in scope.metrics() {
                    received.push((metric.name().to_owned(), metric.unit().to_owned()));
                }
            }
            std::future::ready(Ok(()))
        }

        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }

        fn shutdown_with_timeout(&self, _timeout: std::time::Duration) -> OTelSdkResult {
            Ok(())
        }

        fn temporality(&self) -> opentelemetry_sdk::metrics::Temporality {
            opentelemetry_sdk::metrics::Temporality::Cumulative
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a test runtime builds")
    }

    /// `toasty` asks `tracing::event_enabled!` about a `toasty::query` warning before every
    /// statement. A stderr filter with a bare `warn` default, such as `RUST_LOG=warn,rift=info`,
    /// enables that probe while the export filter refuses it, which is what a search request
    /// meets on the thread its store query ran on.
    #[test]
    fn every_closed_span_is_exported_after_a_probe_the_export_filter_refuses() {
        let runtime = runtime();
        let _entered = runtime.enter();
        let exporter = RecordingExporter::default();
        let provider = tracer_provider(exporter.clone(), BatchConfig::default(), resource());
        let stderr = tracing_subscriber::fmt::layer()
            .with_writer(std::io::sink)
            .with_filter(EnvFilter::new("warn,rift=info"));
        let export = export_layer(&provider, EnvFilter::new("rift=info"));
        let subscriber = tracing_subscriber::registry().with(stderr).with(export);
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..SPANS {
                let _ = tracing::event_enabled!(target: "toasty::query", tracing::Level::WARN);
                crate::traced!(component = "search", operation = "search.request", {});
            }
        });
        provider
            .shutdown()
            .expect("the provider flushes on shutdown");
        assert_eq!(
            exporter.count("search.request"),
            SPANS,
            "every closed span must reach the exporter"
        );
    }

    /// The operator's filter names no SDK target, and the report still reaches stderr.
    #[test]
    fn a_full_export_queue_is_reported_on_stderr() {
        let runtime = runtime();
        let _entered = runtime.enter();
        let batch = BatchConfigBuilder::default().with_max_queue_size(1).build();
        let provider = tracer_provider(StalledExporter, batch, resource());
        let stderr = Reports::default();
        let filter = crate::runtime::stderr_filter(EnvFilter::new("rift=info"));
        let subscriber = tracing_subscriber::registry().with(stderr.clone().with_filter(filter));
        tracing::subscriber::with_default(subscriber, || {
            let tracer = provider.tracer("queue");
            for _ in 0..SPANS {
                tracer.start("queued").end();
            }
            // The full queue refuses the shutdown request too; the total is reported first.
            let _ = provider.shutdown();
        });
        assert_eq!(stderr.count("BatchSpanProcessor.SpanDroppingStarted"), 1);
        assert_eq!(stderr.count("BatchSpanProcessor.Shutdown"), 1);
        // The export task takes at most one span off the queue before the stalled export
        // holds it, and the queue keeps at most one more. The provider hands the processor
        // its resource through that same queue when it is built, so a queue the export task
        // has not polled yet still holds the resource and refuses every span.
        let dropped = stderr
            .dropped_spans()
            .expect("the shutdown reports its dropped total");
        let spans = u64::try_from(SPANS).expect("the span count fits in u64");
        assert!(
            (spans - 2..=spans).contains(&dropped),
            "a queue of one behind a stalled export keeps at most two spans: dropped={dropped}, spans={spans}"
        );
    }

    /// A metric exporter whose export never finishes: a collector that accepted the
    /// connection and never answers.
    #[derive(Debug)]
    struct StalledMetricExporter;

    impl opentelemetry_sdk::metrics::exporter::PushMetricExporter for StalledMetricExporter {
        fn export(
            &self,
            _metrics: &opentelemetry_sdk::metrics::data::ResourceMetrics,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            std::future::pending()
        }

        fn force_flush(&self) -> OTelSdkResult {
            Ok(())
        }

        fn shutdown_with_timeout(&self, _timeout: std::time::Duration) -> OTelSdkResult {
            Ok(())
        }

        fn temporality(&self) -> opentelemetry_sdk::metrics::Temporality {
            opentelemetry_sdk::metrics::Temporality::Cumulative
        }
    }

    /// A value an instrument records reaches the meter provider the export installed,
    /// under the instrument's own name and unit, and the shutdown sends it.
    #[test]
    fn every_recorded_instrument_reaches_the_meter_provider() {
        let runtime = runtime();
        let _entered = runtime.enter();
        let exporter = RecordingMetricExporter::default();
        let meters = meter_provider(exporter.clone(), resource());
        let export = OtlpExport::holding(None, Some(meters.clone()));
        export.install_meter();
        crate::traced!(component = "search", operation = "search.request", {});
        assert!(crate::sampler::observe_process(
            crate::sampler::SystemProcessReader::current()
        ));
        meters
            .force_flush()
            .expect("the reader collects and exports");
        let received = exporter
            .metrics
            .lock()
            .expect("the exporter's metrics are not poisoned")
            .clone();
        for (name, unit) in [
            ("traces.span.metrics.calls", "{call}"),
            ("traces.span.metrics.duration", "s"),
            ("process.memory.usage", "By"),
            ("process.cpu.time", "s"),
        ] {
            assert!(
                received.contains(&(name.to_owned(), unit.to_owned())),
                "{name} in {unit} must be exported: {received:?}"
            );
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        assert_eq!(runtime.block_on(export.shutdown(deadline)), Ok(()));
        assert_eq!(
            runtime.block_on(export.shutdown(deadline)),
            Ok(()),
            "a second shutdown finds nothing to shut down"
        );
    }

    /// The bound the shutdown test waits at most past its deadline: thread start and the
    /// answer's wake.
    const SHUTDOWN_SLACK: std::time::Duration = std::time::Duration::from_millis(500);

    /// A collector that never answers holds the final metric export and the span flush;
    /// the shutdown stops waiting at its deadline, long before the export timeout.
    #[test]
    fn a_stalled_collector_ends_the_shutdown_at_its_deadline() {
        let runtime = runtime();
        let _entered = runtime.enter();
        let meters = meter_provider(StalledMetricExporter, resource());
        let tracer = tracer_provider(StalledExporter, BatchConfig::default(), resource());
        tracer.tracer("stalled").start("queued").end();
        let export = OtlpExport::holding(Some(tracer), Some(meters));
        let bound = std::time::Duration::from_millis(200);
        let started = std::time::Instant::now();
        let ended = runtime.block_on(export.shutdown(tokio::time::Instant::now() + bound));
        let elapsed = started.elapsed();
        assert_eq!(ended, Err(ExportShutdownError::TimedOut));
        assert!(
            elapsed >= bound && elapsed < bound + SHUTDOWN_SLACK,
            "the shutdown ends at its deadline: elapsed={elapsed:?}, bound={bound:?}"
        );
        assert!(bound + SHUTDOWN_SLACK < super::OTLP_EXPORT_TIMEOUT);
    }

    /// Every exported span and metric names the service, its version, this process's
    /// instance, and its process identifier.
    #[test]
    fn the_resource_names_the_service_instance_and_process() {
        use opentelemetry::{Key, Value};
        let resource = resource();
        assert_eq!(
            resource.get(&Key::new("service.name")),
            Some(Value::from("rift"))
        );
        assert_eq!(
            resource.get(&Key::new("service.version")),
            Some(Value::from(env!("CARGO_PKG_VERSION")))
        );
        assert_eq!(
            resource.get(&Key::new("process.pid")),
            Some(Value::I64(i64::from(std::process::id())))
        );
        let instance = resource
            .get(&Key::new("service.instance.id"))
            .expect("the resource carries an instance identifier")
            .to_string();
        let groups: Vec<usize> = instance.split('-').map(str::len).collect();
        assert_eq!(groups, [8, 4, 4, 4, 12], "{instance}");
        assert_eq!(&instance[14..15], "4", "version 4: {instance}");
        assert_ne!(
            super::instance_id(),
            Some(instance),
            "each call draws a new identifier"
        );
    }
}
