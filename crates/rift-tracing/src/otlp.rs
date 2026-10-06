//! The OTLP export of Rift's `tracing` spans, metrics, and log records.
//!
//! Every build carries the export, and a process exports nothing until an operator sets an
//! OTLP endpoint variable for an OTLP/HTTP receiver. Spans export when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set;
//! metrics when it or `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` is; log records when it or
//! `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` is.
//!
//! Every exported span, metric, and log record carries the resource attributes
//! `service.name` (`rift`), `service.version` (the workspace version), `service.instance.id`
//! (random per process), and `process.pid`, so a collector that receives from several
//! servers tells them apart.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, UNIX_EPOCH};

use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _, Severity};
use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
use opentelemetry::{Key, KeyValue};
use opentelemetry_otlp::{
    LogExporter, MetricExporter, Protocol, SpanExporter, WithExportConfig as _,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkError;
use opentelemetry_sdk::logs::log_processor_with_async_runtime::BatchLogProcessor;
use opentelemetry_sdk::logs::{SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
use opentelemetry_sdk::runtime;
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::{BatchConfig, BatchConfigBuilder, SdkTracerProvider};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{FilterExt as _, LevelFilter, Targets};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::record::LogRecord;

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

/// Most time one export of metrics, of a span batch, or of a log record batch takes before
/// the SDK gives it up, when `OTEL_METRIC_EXPORT_TIMEOUT`, `OTEL_BSP_EXPORT_TIMEOUT`, or
/// `OTEL_BLRP_EXPORT_TIMEOUT` sets none: the SDK's own default is 30 s. A given-up export is
/// reported on stderr; the next cumulative export sends its metric points again, and its
/// spans and log records are lost.
pub(crate) const OTLP_EXPORT_TIMEOUT: Duration = Duration::from_secs(2);
/// The variable the SDK reads the metric export timeout from.
const METRIC_EXPORT_TIMEOUT_VAR: &str = "OTEL_METRIC_EXPORT_TIMEOUT";
/// The variable the SDK reads the span export timeout from.
const SPAN_EXPORT_TIMEOUT_VAR: &str = "OTEL_BSP_EXPORT_TIMEOUT";
/// The variable the SDK reads the log record export timeout from.
const LOG_EXPORT_TIMEOUT_VAR: &str = "OTEL_BLRP_EXPORT_TIMEOUT";

/// The target the OpenTelemetry SDK's own reports carry.
pub(crate) const SDK_TARGET: &str = "opentelemetry_sdk";

/// The targets of the crates an export runs through: the OpenTelemetry API, SDK, and OTLP
/// exporter, and the HTTP client stack that sends each batch. Neither export layer hands
/// an event or span of these targets, or of a module below one, to its exporter, whatever
/// filter admits it; stderr keeps them under its own filter.
///
/// An export's own records would otherwise be exported in turn: `opentelemetry` 0.33 names
/// this "telemetry-induced-telemetry" and its consequence "Infinite telemetry feedback
/// loops" and "Excessive resource consumption" (`Context::enter_telemetry_suppressed_scope`,
/// `src/context.rs`), and the async-runtime batch processors this export uses enter no
/// suppressed scope, nor does a request task the HTTP client spawns inherit one.
pub(crate) const EXPORT_OWN_TARGETS: [&str; 10] = [
    "opentelemetry",
    "opentelemetry_sdk",
    "opentelemetry_otlp",
    "opentelemetry_http",
    "reqwest",
    "hyper",
    "hyper_util",
    "h2",
    "tower",
    "native_tls",
];

/// Whether `target` is one of [`EXPORT_OWN_TARGETS`] or a module below one.
fn export_own(target: &str) -> bool {
    EXPORT_OWN_TARGETS.iter().any(|own| {
        target
            .strip_prefix(own)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

/// A filter refusing every span and event of [`EXPORT_OWN_TARGETS`].
fn export_others() -> tracing_subscriber::filter::FilterFn<impl Fn(&tracing::Metadata<'_>) -> bool>
{
    tracing_subscriber::filter::filter_fn(|metadata| !export_own(metadata.target()))
}

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

/// The installed exporter's tracer, meter, and logger providers, shut down at most once.
struct Providers {
    tracer: Option<SdkTracerProvider>,
    meters: Option<SdkMeterProvider>,
    logs: Option<LoggerExport>,
}

/// The logger provider and the gate the log record layer emits through.
///
/// The SDK's logger keeps handing records to its batch processor after the provider shut
/// down, and the processor counts each one dropped and reports the first as a warning. The
/// shutdown closes `open` first, so a record written after it reaches stderr and the store
/// alone and the stop reports no drop.
struct LoggerExport {
    provider: SdkLoggerProvider,
    open: Arc<AtomicBool>,
}

/// The process's OTLP export: the installed tracer, meter, and logger providers, held so the
/// process flushes and shuts them down before it exits.
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
            .map(|providers| {
                (
                    providers.tracer.is_some(),
                    providers.meters.is_some(),
                    providers.logs.is_some(),
                )
            });
        formatter
            .debug_struct("OtlpExport")
            .field("tracer_meters_and_logs", &held)
            .finish()
    }
}

/// Why an export shutdown did not end cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportShutdownError {
    /// The deadline passed first. The shutdown keeps running on its own thread until the
    /// process exits, and the points, spans, and log records it had not sent are lost.
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
    /// An export that holds `tracer`, `meters`, and `logs`.
    fn holding(
        tracer: Option<SdkTracerProvider>,
        meters: Option<SdkMeterProvider>,
        logs: Option<LoggerExport>,
    ) -> Self {
        Self {
            providers: Arc::new(Mutex::new(Some(Providers {
                tracer,
                meters,
                logs,
            }))),
        }
    }

    /// Makes the meter provider the one every instrument's scope builds its meter from, when
    /// one exports. The runtime calls it once its subscriber is installed.
    pub(crate) fn install_meter(&self) {
        let providers = self
            .providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(meters) = providers.as_ref().and_then(|held| held.meters.as_ref()) {
            crate::metrics::install_meter(meters.clone());
        }
    }

    /// The meter provider the recorder reads and installs, with both local and OTLP
    /// readers when an OTLP metric endpoint is configured.
    #[cfg(any(test, feature = "fixtures"))]
    pub(crate) fn recorder_meter_provider(&self) -> Option<SdkMeterProvider> {
        self.providers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(|providers| providers.meters.clone())
    }

    /// Flushes buffered spans, log records, and the final metric points, and shuts every
    /// provider down, by `deadline`. A log record written after this call starts is not
    /// exported.
    ///
    /// The SDK's shutdown calls block and the async-runtime reader and batch processors
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
        let Some(Providers {
            tracer,
            meters,
            logs,
        }) = taken
        else {
            return Ok(());
        };
        let tracer = tracer.map(|tracer| shut_down_on_thread(move || tracer.shutdown()));
        let meters = meters.map(|meters| shut_down_on_thread(move || meters.shutdown()));
        let logs = logs.map(|logs| {
            logs.open.store(false, Ordering::Release);
            shut_down_on_thread(move || logs.provider.shutdown())
        });
        let waited = tokio::time::timeout_at(deadline, async move {
            let mut failures = Vec::new();
            for answer in [tracer, meters, logs].into_iter().flatten() {
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
/// The variables that name where log records export: the logs endpoint, used as it is, or
/// the base endpoint, which the exporter extends with `/v1/logs`.
const LOG_ENDPOINT_VARS: [&str; 2] = [
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
];

/// The variable that disables every export: the OpenTelemetry specification's "Disable
/// the SDK for all signals", where "true", in any case, means "a no-op SDK implementation
/// will be used for all telemetry signals" and "Any other value or absence of the variable
/// will have no effect". `opentelemetry_sdk` 0.33 reads no such variable itself.
const SDK_DISABLED_VAR: &str = "OTEL_SDK_DISABLED";

/// Whether the process sets `variable` and leaves the export enabled: under
/// [`SDK_DISABLED_VAR`] set to `true` no endpoint variable counts, so nothing exports.
fn configured(variable: &str) -> bool {
    std::env::var_os(variable).is_some() && !sdk_disabled()
}

/// Whether this process needs a Tokio runtime to build its test OTLP exporters.
#[cfg(any(test, feature = "fixtures"))]
pub(crate) fn recorder_export_configured() -> bool {
    !sdk_disabled()
        && [
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
            "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
        ]
        .iter()
        .any(|variable| std::env::var_os(variable).is_some())
}

/// Whether [`SDK_DISABLED_VAR`] reads `true`, in any case.
fn sdk_disabled() -> bool {
    disables_sdk(std::env::var(SDK_DISABLED_VAR).ok().as_deref())
}

/// Whether `value`, the text of [`SDK_DISABLED_VAR`] or `None` when it is unset or not
/// Unicode, disables the export: `true` in any case, around blanks; any other value or
/// none does not.
fn disables_sdk(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

/// The resource every exported span, metric, and log record carries: `service.name`,
/// `service.version`, a random `service.instance.id` in the UUID version 4 form, and
/// `process.pid`.
fn resource() -> Resource {
    let mut builder = Resource::builder()
        .with_service_name(SERVICE_NAME)
        .with_attribute(KeyValue::new("service.version", SERVICE_VERSION));
    if let Ok(test_case) = std::env::var("NEXTEST_ATTEMPT_ID") {
        builder = builder.with_attribute(KeyValue::new("test.case.name", test_case));
    }
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

/// Installs an OTLP/HTTP span export layer when `OTEL_EXPORTER_OTLP_ENDPOINT` names a
/// collector, `None` otherwise, beside a meter provider when a metric endpoint variable
/// names one, and a log record export layer under `log_filter` when a log endpoint variable
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
pub(crate) fn layer<S>(
    log_filter: tracing_subscriber::EnvFilter,
) -> (impl Layer<S> + Send + Sync, OtlpExport)
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
{
    layer_inner(log_filter, meter_provider)
}

/// The recorder's OTLP layer, with an in-memory reader on its meter provider so local
/// metric assertions continue to read the SDK's points.
#[cfg(any(test, feature = "fixtures"))]
pub(crate) fn recorder_layer<S>(
    log_filter: tracing_subscriber::EnvFilter,
    local_metrics: opentelemetry_sdk::metrics::InMemoryMetricExporter,
) -> (impl Layer<S> + Send + Sync, OtlpExport)
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
{
    layer_inner(log_filter, move |exporter, resource| {
        meter_provider_with_local_reader(exporter, resource, local_metrics)
    })
}

fn layer_inner<S>(
    log_filter: tracing_subscriber::EnvFilter,
    make_meter_provider: impl FnOnce(MetricExporter, Resource) -> SdkMeterProvider,
) -> (impl Layer<S> + Send + Sync, OtlpExport)
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
{
    let resource = resource();
    let logs = if LOG_ENDPOINT_VARS
        .iter()
        .any(|variable| configured(variable))
    {
        match LogExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()
        {
            Ok(exporter) => Some(logger_export(
                exporter,
                log_batch_config(),
                resource.clone(),
            )),
            Err(error) => {
                eprintln!("rift: warning: otlp log exporter did not build: {error}");
                None
            }
        }
    } else {
        None
    };
    let meters = if METRIC_ENDPOINT_VARS
        .iter()
        .any(|variable| configured(variable))
    {
        match MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()
        {
            Ok(exporter) => Some(make_meter_provider(exporter, resource.clone())),
            Err(error) => {
                eprintln!("rift: warning: otlp metric exporter did not build: {error}");
                None
            }
        }
    } else {
        None
    };
    let tracer = if configured("OTEL_EXPORTER_OTLP_ENDPOINT") {
        match SpanExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()
        {
            Ok(exporter) => Some(tracer_provider(exporter, batch_config(), resource)),
            Err(error) => {
                eprintln!("rift: warning: otlp exporter did not build: {error}");
                None
            }
        }
    } else {
        None
    };
    let span_layer = tracer.as_ref().map(|provider| {
        let filter = std::env::var(RIFT_OTLP_FILTER_VAR)
            .ok()
            .and_then(|value| tracing_subscriber::EnvFilter::try_new(value).ok())
            .unwrap_or_else(|| tracing_subscriber::EnvFilter::new(DEFAULT_OTLP_FILTER));
        export_layer(provider, filter)
    });
    let log_layer = logs.as_ref().map(|logs| log_record_layer(logs, log_filter));
    // The log record layer runs first: a span's close reaches it while the span export
    // still holds the span's trace and span identifiers.
    (
        Layer::<S>::and_then(log_layer, span_layer),
        OtlpExport::holding(tracer, meters, logs),
    )
}

/// The log record batch processor's settings: the SDK's, read from the `OTEL_BLRP_*`
/// variables, with [`OTLP_EXPORT_TIMEOUT`] when `OTEL_BLRP_EXPORT_TIMEOUT` sets no export
/// timeout.
fn log_batch_config() -> opentelemetry_sdk::logs::BatchConfig {
    let builder = opentelemetry_sdk::logs::BatchConfigBuilder::default();
    if configured(LOG_EXPORT_TIMEOUT_VAR) {
        builder.build()
    } else {
        builder.with_max_export_timeout(OTLP_EXPORT_TIMEOUT).build()
    }
}

/// The logger provider that batches every emitted log record into `exporter` under `batch`,
/// with its gate open.
///
/// The batch processor is the SDK's async-runtime one, for the reason the span batches use
/// it: the OTLP exporter posts over the async `reqwest` client, which needs a Tokio reactor.
/// Must be called inside a Tokio runtime: the processor spawns its export task there.
fn logger_export<E>(
    exporter: E,
    batch: opentelemetry_sdk::logs::BatchConfig,
    resource: Resource,
) -> LoggerExport
where
    E: opentelemetry_sdk::logs::LogExporter + 'static,
{
    let processor = BatchLogProcessor::builder(exporter, runtime::Tokio)
        .with_batch_config(batch)
        .build();
    LoggerExport {
        provider: SdkLoggerProvider::builder()
            .with_resource(resource)
            .with_log_processor(processor)
            .build(),
        open: Arc::new(AtomicBool::new(true)),
    }
}

/// The layer that hands every record `filter` admits to `logs`, as the log store would
/// keep it.
///
/// `filter` is reevaluated at every span and event, as the capture's is, so the export
/// carries what a capture under the same filter records.
fn log_record_layer<S>(
    logs: &LoggerExport,
    filter: tracing_subscriber::EnvFilter,
) -> impl Layer<S> + Send + Sync + use<S>
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
{
    LogRecordExport {
        logger: logs.provider.logger(SERVICE_NAME),
        open: Arc::clone(&logs.open),
    }
    .with_filter(crate::runtime::reevaluated(filter).and(export_others()))
}

/// The `tracing` layer that exports each event and each span close as an OTLP log record.
///
/// It builds the record the capture builds, through [`crate::capture::event_record`] and
/// [`crate::capture::closed_record`], so an exported record carries what a stored one
/// carries: `component` and `operation`, inherited from the nearest span that names them;
/// every other field; and `root_span` and `nearest_span` for the spans around an event. A
/// span close is exported too, and its fields carry `elapsed_ms` and `status.code`.
/// `opentelemetry-appender-tracing` maps an event's own fields alone and exports no span
/// close, so it is not used.
struct LogRecordExport {
    logger: SdkLogger,
    open: Arc<AtomicBool>,
}

impl<S> Layer<S> for LogRecordExport
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span>,
{
    /// The SDK's logger takes the trace and span identifiers from the OpenTelemetry context
    /// current on this thread: the one `tracing-opentelemetry` attaches when the event's
    /// span is entered.
    fn on_event(&self, event: &tracing::Event<'_>, context: Context<'_, S>) {
        if !self.open.load(Ordering::Acquire) {
            return;
        }
        let record = crate::capture::event_record(event, &context);
        self.emit(*event.metadata().level(), &record, None);
    }

    /// A closing span is no longer entered, so its own trace and span identifiers are read
    /// from `tracing-opentelemetry`'s data for it. This layer runs before the span export
    /// layer, whose close ends that data.
    fn on_close(&self, id: tracing::span::Id, context: Context<'_, S>) {
        if !self.open.load(Ordering::Acquire) {
            return;
        }
        let Some(level) = context.metadata(&id).map(|metadata| *metadata.level()) else {
            return;
        };
        let Some(record) = crate::capture::closed_record(&id, &context) else {
            return;
        };
        let span = tracing::dispatcher::get_default(|dispatch| {
            tracing_opentelemetry::get_otel_context(&id, dispatch)
        });
        self.emit(level, &record, span.as_ref());
    }
}

impl LogRecordExport {
    /// Emits `record` at `level`, under the trace and span identifiers of `span` when given.
    ///
    /// The record's target becomes the OTLP instrumentation scope name, its message the
    /// body, and `component`, `operation`, and each member of its fields an attribute.
    fn emit(
        &self,
        level: tracing::Level,
        record: &LogRecord,
        span: Option<&opentelemetry::Context>,
    ) {
        let mut exported = self.logger.create_log_record();
        let recorded_at_ms = u64::try_from(record.recorded_at_ms()).unwrap_or(0);
        exported.set_timestamp(UNIX_EPOCH + Duration::from_millis(recorded_at_ms));
        exported.set_target(record.target().to_owned());
        exported.set_severity_number(severity(level));
        exported.set_severity_text(level.as_str());
        exported.set_body(AnyValue::from(record.message().to_owned()));
        for (label, value) in [
            ("component", record.component()),
            ("operation", record.operation()),
        ] {
            if !value.is_empty() {
                exported.add_attribute(label, value.to_owned());
            }
        }
        exported.add_attributes(field_attributes(record.fields()));
        if let Some(span) = span {
            let span = span.span();
            let span_context = span.span_context();
            if span_context.is_valid() {
                exported.set_trace_context(
                    span_context.trace_id(),
                    span_context.span_id(),
                    Some(span_context.trace_flags()),
                );
            }
        }
        self.logger.emit(exported);
    }
}

/// The OpenTelemetry severity of a `tracing` level.
const fn severity(level: tracing::Level) -> Severity {
    match level {
        tracing::Level::TRACE => Severity::Trace,
        tracing::Level::DEBUG => Severity::Debug,
        tracing::Level::INFO => Severity::Info,
        tracing::Level::WARN => Severity::Warn,
        tracing::Level::ERROR => Severity::Error,
    }
}

/// The attributes of a record's fields: one per member of the JSON object, an object member
/// such as `root_span` as a map. Fields cut at their bound are no longer a JSON object, and
/// export whole as the one attribute `fields`.
fn field_attributes(fields: &str) -> Vec<(Key, AnyValue)> {
    match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(fields) {
        Ok(members) => members
            .into_iter()
            .map(|(name, value)| (Key::new(name), any_value(value)))
            .collect(),
        Err(_) => vec![(Key::new("fields"), AnyValue::from(fields.to_owned()))],
    }
}

/// `value` as an OpenTelemetry attribute value. The capture writes every field value as a
/// string and every span member as an object; any other value exports as its JSON text.
fn any_value(value: serde_json::Value) -> AnyValue {
    match value {
        serde_json::Value::String(text) => AnyValue::from(text),
        serde_json::Value::Object(members) => members
            .into_iter()
            .map(|(name, value)| (Key::new(name), any_value(value)))
            .collect(),
        other => AnyValue::from(other.to_string()),
    }
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

/// The recorder meter provider with an additional in-memory reader for local assertions.
#[cfg(any(test, feature = "fixtures"))]
fn meter_provider_with_local_reader<E>(
    exporter: E,
    resource: Resource,
    local_metrics: opentelemetry_sdk::metrics::InMemoryMetricExporter,
) -> SdkMeterProvider
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
        .with_reader(opentelemetry_sdk::metrics::PeriodicReader::builder(local_metrics).build())
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
        .with_filter(crate::runtime::reevaluated(filter).and(export_others()))
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

    use opentelemetry::Key;
    use opentelemetry::logs::{AnyValue, Severity};
    use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLogRecord};
    use opentelemetry_sdk::trace::InMemorySpanExporter;

    use super::{
        ExportShutdownError, LOG_ENDPOINT_VARS, OtlpExport, configured, export_layer,
        field_attributes, log_batch_config, log_record_layer, logger_export, meter_provider,
        resource, tracer_provider,
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

    /// Reads the dropped total of one event: `dropped_spans` of the span batch processor,
    /// `dropped_logs_count` of the log record one.
    #[derive(Default)]
    struct DroppedSpans(Option<u64>);

    impl Visit for DroppedSpans {
        fn record_u64(&mut self, field: &Field, value: u64) {
            if matches!(field.name(), "dropped_spans" | "dropped_logs_count") {
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
        let export = OtlpExport::holding(None, Some(meters.clone()), None);
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
        let export = OtlpExport::holding(Some(tracer), Some(meters), None);
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

    /// Records and spans the batch queue of a refused-collector case holds.
    const REFUSED_QUEUE: usize = 4;
    /// Records and spans one export of a refused-collector case carries, at most.
    const REFUSED_BATCH: usize = 2;
    /// How often a refused-collector case's batch processor exports.
    const REFUSED_DELAY: std::time::Duration = std::time::Duration::from_millis(20);
    /// How long the refusing collector takes to answer: far longer than one burst of
    /// emits, so a burst meets an export in progress and a full queue.
    const REFUSAL_TIME: std::time::Duration = std::time::Duration::from_millis(50);
    /// Bursts, one per wait, a refused-collector case emits.
    const REFUSED_BURSTS: usize = 5;
    /// Records or spans one burst emits: four times what the queue, the batch being
    /// gathered, and the export in progress hold together.
    const REFUSED_BURST: usize = 4 * (REFUSED_QUEUE + 2 * REFUSED_BATCH);
    /// The wait after each burst: two refusals.
    const REFUSED_WAIT: std::time::Duration = std::time::Duration::from_millis(100);

    /// A collector that answers every export with a refusal, as one answering 503 does,
    /// after [`REFUSAL_TIME`]; it counts what it was handed and the largest batch.
    #[derive(Clone, Debug, Default)]
    struct RefusingExporter {
        handed: Arc<std::sync::atomic::AtomicUsize>,
        largest: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl RefusingExporter {
        fn refuse(
            &self,
            batch: usize,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send + use<> {
            use std::sync::atomic::Ordering;
            self.handed.fetch_add(batch, Ordering::SeqCst);
            self.largest.fetch_max(batch, Ordering::SeqCst);
            async {
                tokio::time::sleep(REFUSAL_TIME).await;
                Err(opentelemetry_sdk::error::OTelSdkError::InternalFailure(
                    "503 Service Unavailable".to_owned(),
                ))
            }
        }

        fn handed(&self) -> usize {
            self.handed.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn largest(&self) -> usize {
            self.largest.load(std::sync::atomic::Ordering::SeqCst)
        }

        /// Waits, [`REFUSED_WAIT`] at a time, until two waits in a row hand it nothing:
        /// the queue is drained, so the shutdown request finds room in it.
        fn settle(&self) {
            let mut seen = self.handed();
            for _ in 0..REFUSED_BURSTS * REFUSED_BURST {
                std::thread::sleep(2 * REFUSED_WAIT);
                let now = self.handed();
                if now == seen {
                    return;
                }
                seen = now;
            }
        }
    }

    impl SpanExporter for RefusingExporter {
        fn export(
            &self,
            batch: Vec<SpanData>,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            self.refuse(batch.len())
        }
    }

    impl opentelemetry_sdk::logs::LogExporter for RefusingExporter {
        fn export(
            &self,
            batch: opentelemetry_sdk::logs::LogBatch<'_>,
        ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
            self.refuse(batch.iter().count())
        }
    }

    /// What a refused-collector case asserts once its processor shut down: every export
    /// carried at most a batch; each burst got at most what the queue, the batch being
    /// gathered, and the export in progress hold past the full queue; and every emitted
    /// item was either handed to the collector or counted in the dropped total the SDK
    /// reports at its shutdown.
    fn assert_bounded_and_counted(exporter: &RefusingExporter, reports: &Reports) {
        let emitted = REFUSED_BURSTS * REFUSED_BURST;
        let handed = exporter.handed();
        let dropped = reports
            .dropped_spans()
            .and_then(|dropped| usize::try_from(dropped).ok())
            .expect("the shutdown reports its dropped total");
        assert!(
            exporter.largest() <= REFUSED_BATCH,
            "{}",
            exporter.largest()
        );
        assert!(
            handed <= REFUSED_BURSTS * (REFUSED_QUEUE + 2 * REFUSED_BATCH),
            "the queue bounds what each burst gets past it: handed={handed}"
        );
        assert_eq!(
            handed + dropped,
            emitted,
            "every item is handed or counted dropped: handed={handed}, dropped={dropped}"
        );
    }

    /// A collector refusing every span batch over several export intervals keeps the span
    /// queue at its bound, and the SDK counts each span the full queue refused.
    #[test]
    fn a_refusing_collector_keeps_the_span_queue_bounded_and_counts_the_drops() {
        let runtime = runtime();
        let _entered = runtime.enter();
        let exporter = RefusingExporter::default();
        let batch = BatchConfigBuilder::default()
            .with_max_queue_size(REFUSED_QUEUE)
            .with_max_export_batch_size(REFUSED_BATCH)
            .with_scheduled_delay(REFUSED_DELAY)
            .build();
        let provider = tracer_provider(exporter.clone(), batch, resource());
        let reports = Reports::default();
        let filter = crate::runtime::stderr_filter(EnvFilter::new("rift=info"));
        let subscriber = tracing_subscriber::registry().with(reports.clone().with_filter(filter));
        tracing::subscriber::with_default(subscriber, || {
            let tracer = provider.tracer("refused");
            for _ in 0..REFUSED_BURSTS {
                for _ in 0..REFUSED_BURST {
                    tracer.start("refused").end();
                }
                std::thread::sleep(REFUSED_WAIT);
            }
            exporter.settle();
            provider
                .shutdown()
                .expect("the drained queue takes the shutdown");
        });
        assert_eq!(reports.count("BatchSpanProcessor.SpanDroppingStarted"), 1);
        assert_bounded_and_counted(&exporter, &reports);
    }

    /// A collector refusing every log record batch over several export intervals keeps
    /// the log record queue at its bound, and the SDK counts each record the full queue
    /// refused.
    #[test]
    fn a_refusing_collector_keeps_the_log_queue_bounded_and_counts_the_drops() {
        use opentelemetry::logs::{LogRecord as _, Logger as _, LoggerProvider as _};
        let runtime = runtime();
        let _entered = runtime.enter();
        let exporter = RefusingExporter::default();
        let batch = opentelemetry_sdk::logs::BatchConfigBuilder::default()
            .with_max_queue_size(REFUSED_QUEUE)
            .with_max_export_batch_size(REFUSED_BATCH)
            .with_scheduled_delay(REFUSED_DELAY)
            .build();
        let logs = logger_export(exporter.clone(), batch, resource());
        let reports = Reports::default();
        let filter = crate::runtime::stderr_filter(EnvFilter::new("rift=info"));
        let subscriber = tracing_subscriber::registry().with(reports.clone().with_filter(filter));
        tracing::subscriber::with_default(subscriber, || {
            let logger = logs.provider.logger("refused");
            for _ in 0..REFUSED_BURSTS {
                for _ in 0..REFUSED_BURST {
                    let mut record = logger.create_log_record();
                    record.set_body(AnyValue::from("refused".to_owned()));
                    logger.emit(record);
                }
                std::thread::sleep(REFUSED_WAIT);
            }
            exporter.settle();
            logs.provider
                .shutdown()
                .expect("the drained queue takes the shutdown");
        });
        assert_eq!(reports.count("BatchLogProcessor.LogDroppingStarted"), 1);
        assert_bounded_and_counted(&exporter, &reports);
    }

    /// The attribute `key` of `record`, when it carries one.
    fn attribute<'record>(record: &'record SdkLogRecord, key: &str) -> Option<&'record AnyValue> {
        record
            .attributes_iter()
            .find(|(name, _)| name.as_str() == key)
            .map(|(_, value)| value)
    }

    /// The exported record whose body is `body`.
    fn exported<'records>(records: &'records [SdkLogRecord], body: &str) -> &'records SdkLogRecord {
        records
            .iter()
            .find(|record| record.body() == Some(&AnyValue::from(body.to_owned())))
            .unwrap_or_else(|| panic!("a record with the body {body:?} is exported: {records:?}"))
    }

    /// An event inside a span exports as the store keeps it - level, message, its own
    /// fields, the span's `component` and `operation`, the `root_span` member - under the
    /// span's trace and span identifiers, and the span's close exports under the same.
    #[test]
    fn an_event_and_its_span_close_export_as_log_records_in_the_span() {
        let runtime = runtime();
        let _entered = runtime.enter();
        let spans = InMemorySpanExporter::default();
        let tracer = tracer_provider(spans.clone(), BatchConfig::default(), resource());
        let records = InMemoryLogExporter::default();
        let logs = logger_export(records.clone(), log_batch_config(), resource());
        let subscriber = crate::capture::registry()
            .with(log_record_layer(&logs, EnvFilter::new("rift_tracing=info")))
            .with(export_layer(&tracer, EnvFilter::new("rift_tracing=info")));
        tracing::subscriber::with_default(subscriber, || {
            crate::traced!(
                component = "search",
                operation = "search.request",
                root = "/workspace",
                {
                    crate::info!(request_id = "r-1", hits = 3_u64, "search answered");
                }
            );
        });
        tracer.force_flush().expect("the span batch flushes");
        logs.provider.force_flush().expect("the log batch flushes");
        let span = spans
            .get_finished_spans()
            .expect("the span exporter is readable")
            .into_iter()
            .find(|span| span.name == "search.request")
            .expect("the span is exported");
        let records: Vec<SdkLogRecord> = records
            .get_emitted_logs()
            .expect("the log exporter is readable")
            .into_iter()
            .map(|exported| exported.record)
            .collect();

        let event = exported(&records, "search answered");
        assert_eq!(event.severity_number(), Some(Severity::Info));
        assert_eq!(event.severity_text(), Some("INFO"));
        assert_eq!(
            event.target().map(AsRef::as_ref),
            Some("rift_tracing::otlp::tests")
        );
        for (key, value) in [
            ("component", "search"),
            ("operation", "search.request"),
            ("request_id", "r-1"),
            ("hits", "3"),
        ] {
            assert_eq!(
                attribute(event, key),
                Some(&AnyValue::from(value.to_owned())),
                "{key}: {event:?}"
            );
        }
        let Some(AnyValue::Map(root_span)) = attribute(event, "root_span") else {
            panic!("the event carries its root span as a map: {event:?}");
        };
        assert_eq!(
            root_span.get(&Key::new("name")),
            Some(&AnyValue::from("search.request".to_owned()))
        );
        let Some(AnyValue::Map(span_fields)) = root_span.get(&Key::new("fields")) else {
            panic!("the root span carries its fields as a map: {root_span:?}");
        };
        assert_eq!(
            span_fields.get(&Key::new("root")),
            Some(&AnyValue::from("/workspace".to_owned()))
        );
        let in_span = event
            .trace_context()
            .expect("the event carries the span's trace context");
        assert_eq!(in_span.trace_id, span.span_context.trace_id());
        assert_eq!(in_span.span_id, span.span_context.span_id());

        let close = exported(&records, "search.request");
        assert_eq!(
            attribute(close, "status.code"),
            Some(&AnyValue::from("Ok".to_owned()))
        );
        assert_eq!(
            attribute(close, "root"),
            Some(&AnyValue::from("/workspace".to_owned()))
        );
        let closed = close
            .trace_context()
            .expect("the close carries the span's own trace context");
        assert_eq!(closed.trace_id, span.span_context.trace_id());
        assert_eq!(closed.span_id, span.span_context.span_id());

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let export = OtlpExport::holding(Some(tracer), None, Some(logs));
        assert_eq!(runtime.block_on(export.shutdown(deadline)), Ok(()));
    }

    /// A record written after the shutdown started is not exported, and the batch processor
    /// that already stopped reports no drop on stderr.
    ///
    /// The subscriber is the process's global one, as the runtime installs it: under a
    /// scoped one, `tracing` hands an event emitted inside a layer's `on_event` to no
    /// subscriber, and the SDK's report of the drop would be lost either way.
    #[test]
    fn a_record_after_the_shutdown_reports_no_drop() {
        let runtime = runtime();
        let _entered = runtime.enter();
        let records = InMemoryLogExporter::default();
        let logs = logger_export(records.clone(), log_batch_config(), resource());
        let layer = log_record_layer(&logs, EnvFilter::new("rift_tracing=info"));
        let export = OtlpExport::holding(None, None, Some(logs));
        let stderr = Reports::default();
        let filter = crate::runtime::stderr_filter(EnvFilter::new("rift=info"));
        let subscriber = crate::capture::registry()
            .with(layer)
            .with(stderr.clone().with_filter(filter));
        tracing::subscriber::set_global_default(subscriber)
            .expect("this case owns the process's subscriber");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        assert_eq!(runtime.block_on(export.shutdown(deadline)), Ok(()));
        // The batch processor answers its shutdown before its task ends and drops the queue;
        // a record written after that finds the queue closed.
        std::thread::sleep(std::time::Duration::from_millis(50));
        crate::info!(component = "mcp", "written after the export stopped");
        assert_eq!(stderr.count("BatchLogProcessor.LogDroppingStarted"), 0);
        assert!(
            records
                .get_emitted_logs()
                .expect("the log exporter is readable")
                .is_empty()
        );
    }

    /// A capture filter naming a bare level admits the export's own crates, and neither
    /// export layer hands their records on: an event of the HTTP client or the SDK, as an
    /// export writes on every batch, never reaches the log record exporter, so a refused or
    /// slow collector cannot feed the next batch with records about the last one.
    #[test]
    fn the_export_never_exports_the_records_of_its_own_crates() {
        let runtime = runtime();
        let _entered = runtime.enter();
        let records = InMemoryLogExporter::default();
        let logs = logger_export(records.clone(), log_batch_config(), resource());
        let subscriber =
            crate::capture::registry().with(log_record_layer(&logs, EnvFilter::new("trace")));
        tracing::subscriber::with_default(subscriber, || {
            tracing::event!(target: "opentelemetry", tracing::Level::WARN, "own");
            tracing::event!(target: "opentelemetry_sdk", tracing::Level::WARN, "own");
            tracing::event!(target: "opentelemetry_otlp", tracing::Level::WARN, "own");
            tracing::event!(target: "opentelemetry_http", tracing::Level::WARN, "own");
            tracing::event!(target: "reqwest::connect", tracing::Level::DEBUG, "own");
            tracing::event!(target: "hyper", tracing::Level::DEBUG, "own");
            tracing::event!(target: "hyper_util::client", tracing::Level::DEBUG, "own");
            tracing::event!(target: "h2::codec", tracing::Level::TRACE, "own");
            tracing::event!(target: "tower", tracing::Level::DEBUG, "own");
            tracing::event!(target: "native_tls", tracing::Level::DEBUG, "own");
            tracing::event!(target: "hyper::client::pool", tracing::Level::DEBUG, "pooled");
            tracing::event!(target: "reqwestish", tracing::Level::INFO, "kept: another crate");
            crate::info!(component = "mcp", "kept: a rift record");
        });
        logs.provider.force_flush().expect("the log batch flushes");
        let exported: Vec<String> = records
            .get_emitted_logs()
            .expect("the log exporter is readable")
            .iter()
            .filter_map(|log| log.record.body().map(|body| format!("{body:?}")))
            .collect();
        assert_eq!(exported.len(), 2, "{exported:?}");
        assert!(
            exported.iter().all(|body| body.contains("kept")),
            "{exported:?}"
        );
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let export = OtlpExport::holding(None, None, Some(logs));
        assert_eq!(runtime.block_on(export.shutdown(deadline)), Ok(()));
    }

    /// Without an endpoint variable the process builds no logger provider, so no record is
    /// handed to a log record exporter.
    #[test]
    fn no_endpoint_installs_no_log_export() {
        assert!(
            !LOG_ENDPOINT_VARS
                .iter()
                .any(|variable| configured(variable)),
            "the test process sets no OTLP log endpoint variable"
        );
        let (_, export) = super::layer::<tracing_subscriber::Registry>(EnvFilter::new("rift=info"));
        let holds_logs = export
            .providers
            .lock()
            .expect("the providers are not poisoned")
            .as_ref()
            .map(|providers| providers.logs.is_some());
        assert_eq!(holds_logs, Some(false));
    }

    /// `OTEL_SDK_DISABLED` disables the export when it reads `true` in any case; any other
    /// value, and its absence, leave the export on.
    #[test]
    fn only_true_in_any_case_disables_the_sdk() {
        for disabling in ["true", "TRUE", "True", " true "] {
            assert!(super::disables_sdk(Some(disabling)), "{disabling:?}");
        }
        for enabling in ["false", "1", "yes", "", "truex"] {
            assert!(!super::disables_sdk(Some(enabling)), "{enabling:?}");
        }
        assert!(!super::disables_sdk(None));
    }

    /// Fields cut at their bound are no longer a JSON object, and export whole.
    #[test]
    fn fields_cut_at_their_bound_export_whole() {
        let cut = "{\"request_id\":\"r-";
        assert_eq!(
            field_attributes(cut),
            vec![(Key::new("fields"), AnyValue::from(cut.to_owned()))]
        );
    }

    /// Every exported signal names its service, version, instance, process, and nextest case.
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
        assert_eq!(
            resource.get(&Key::new("test.case.name")),
            std::env::var("NEXTEST_ATTEMPT_ID").ok().map(Value::from),
            "the resource carries the exact nextest attempt identifier when set"
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
