//! Optional OTLP export of Rift's `tracing` spans and metrics, behind the `otlp` cargo
//! feature.
//!
//! The `rift` binary's `otlp` feature turns this crate's on. Off by default, so a release
//! binary built without `--features otlp` carries no
//! OpenTelemetry export stack. Compiled in, the process still exports nothing until an
//! operator sets an OTLP endpoint variable - for the in-memory collector `just
//! trace-collector` runs, or any other OTLP/HTTP receiver. Spans export when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set; metrics when it or
//! `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` is.

#[cfg(feature = "otlp")]
use opentelemetry::metrics::MeterProvider as _;
#[cfg(feature = "otlp")]
use opentelemetry::trace::TracerProvider as _;
#[cfg(feature = "otlp")]
use opentelemetry_otlp::{MetricExporter, Protocol, SpanExporter, WithExportConfig as _};
#[cfg(feature = "otlp")]
use opentelemetry_sdk::Resource;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::metrics::SdkMeterProvider;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::runtime;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::trace::{BatchConfig, SdkTracerProvider};

use tracing_subscriber::Layer;
use tracing_subscriber::filter::{LevelFilter, Targets};
#[cfg(feature = "otlp")]
use tracing_subscriber::registry::LookupSpan;

/// The `service.name` resource attribute every exported span and metric carries.
#[cfg(feature = "otlp")]
const SERVICE_NAME: &str = "rift";
/// Overrides the export layer's own filter; unset, [`DEFAULT_OTLP_FILTER`] applies.
#[cfg(feature = "otlp")]
const RIFT_OTLP_FILTER_VAR: &str = "RIFT_OTLP_FILTER";
/// Keeps Rift's own crates - the ones `traced!` instruments - at info; a dependency's own
/// spans stay out unless the operator names it.
#[cfg(feature = "otlp")]
const DEFAULT_OTLP_FILTER: &str =
    "rift=info,rift_mcp=info,rift_server=info,rift_index=info,rift_analysis=info";

/// The target the OpenTelemetry SDK's own reports carry.
pub(crate) const SDK_TARGET: &str = "opentelemetry_sdk";

/// The OpenTelemetry SDK's own warnings and errors, which stderr carries whatever
/// `RUST_LOG` names.
///
/// The batch processor queues at most `OTEL_BSP_MAX_QUEUE_SIZE` ended spans, 2048 when the
/// variable is unset, and drops a span it cannot queue. It reports the first drop and the
/// dropped total at shutdown as warnings, and a failed export as an error, so a full queue
/// or an unreachable collector reaches the operator instead of thinning the trace unseen.
/// A build without the `otlp` feature links no SDK, and the target matches nothing.
pub(crate) fn sdk_reports() -> Targets {
    Targets::new().with_target(SDK_TARGET, LevelFilter::WARN)
}

/// The installed exporter's tracer and meter providers, held so the caller can flush and
/// shut them down before the process exits.
///
/// Holds nothing when the `otlp` feature is not compiled in, or when no collector
/// endpoint was configured; [`Export::shutdown`] is then a no-op.
pub(crate) struct Export {
    #[cfg(feature = "otlp")]
    provider: Option<SdkTracerProvider>,
    #[cfg(feature = "otlp")]
    meters: Option<SdkMeterProvider>,
}

impl Export {
    /// Flushes buffered spans and shuts the tracer provider down.
    ///
    /// A collector that is unreachable at shutdown is not this process's failure: the
    /// server has already finished serving, and losing the last batch of spans must not
    /// turn a clean run into a nonzero exit status.
    #[cfg_attr(
        not(feature = "otlp"),
        expect(
            clippy::unused_self,
            reason = "a build without the otlp feature holds no provider to shut down"
        )
    )]
    pub(crate) fn shutdown(self) {
        #[cfg(feature = "otlp")]
        if let Some(provider) = self.provider
            && let Err(error) = provider.shutdown()
        {
            eprintln!("rift: warning: otlp shutdown failed: {error}");
        }
        #[cfg(feature = "otlp")]
        if let Some(meters) = self.meters
            && let Err(error) = meters.shutdown()
        {
            eprintln!("rift: warning: otlp metric shutdown failed: {error}");
        }
    }

    /// Makes the meter provider's meter the one every instrument records into, when one
    /// exports. The runtime calls it once its subscriber is installed.
    #[cfg_attr(
        not(feature = "otlp"),
        expect(
            clippy::unused_self,
            reason = "a build without the otlp feature holds no meter provider"
        )
    )]
    pub(crate) fn install_meter(&self) {
        #[cfg(feature = "otlp")]
        if let Some(meters) = &self.meters {
            crate::metrics::install_meter(meters.meter_with_scope(crate::metrics::scope()));
        }
    }
}

/// The variables that name where metrics export: the metrics endpoint, used as it is, or
/// the base endpoint, which the exporter extends with `/v1/metrics`.
#[cfg(feature = "otlp")]
const METRIC_ENDPOINT_VARS: [&str; 2] = [
    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
];

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
/// building anything keeps the feature from silently dialing OTLP's default
/// `http://localhost:4318` the moment it is compiled in.
#[cfg(feature = "otlp")]
pub(crate) fn layer<S>() -> (Option<impl Layer<S> + Send + Sync>, Export)
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
{
    let configured = |variable: &str| std::env::var_os(variable).is_some();
    let meters = if METRIC_ENDPOINT_VARS
        .iter()
        .any(|variable| configured(variable))
    {
        match MetricExporter::builder()
            .with_http()
            .with_protocol(Protocol::HttpBinary)
            .build()
        {
            Ok(exporter) => Some(meter_provider(exporter)),
            Err(error) => {
                eprintln!("rift: warning: otlp metric exporter did not build: {error}");
                None
            }
        }
    } else {
        None
    };
    if !configured("OTEL_EXPORTER_OTLP_ENDPOINT") {
        return (
            None,
            Export {
                provider: None,
                meters,
            },
        );
    }
    let exporter = match SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()
    {
        Ok(exporter) => exporter,
        Err(error) => {
            eprintln!("rift: warning: otlp exporter did not build: {error}");
            return (
                None,
                Export {
                    provider: None,
                    meters,
                },
            );
        }
    };
    let provider = tracer_provider(exporter, BatchConfig::default());
    let filter = std::env::var(RIFT_OTLP_FILTER_VAR)
        .ok()
        .and_then(|value| tracing_subscriber::EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new(DEFAULT_OTLP_FILTER));
    (
        Some(export_layer(&provider, filter)),
        Export {
            provider: Some(provider),
            meters,
        },
    )
}

/// The meter provider that exports what every instrument records into `exporter`.
///
/// Its reader exports on the Tokio runtime, at `OTEL_METRIC_EXPORT_INTERVAL` or the SDK's
/// 60 s default, so the OTLP exporter posts over the same async `reqwest` client the span
/// batches use. Must be called inside a Tokio runtime.
#[cfg(feature = "otlp")]
fn meter_provider<E>(exporter: E) -> SdkMeterProvider
where
    E: opentelemetry_sdk::metrics::exporter::PushMetricExporter,
{
    let resource = Resource::builder().with_service_name(SERVICE_NAME).build();
    SdkMeterProvider::builder()
        .with_resource(resource)
        .with_reader(PeriodicReader::builder(exporter, runtime::Tokio).build())
        .build()
}

/// The tracer provider that batches every ended span into `exporter` under `batch`.
///
/// Must be called inside a Tokio runtime: the batch processor spawns its export task
/// there.
#[cfg(feature = "otlp")]
fn tracer_provider<E>(exporter: E, batch: BatchConfig) -> SdkTracerProvider
where
    E: opentelemetry_sdk::trace::SpanExporter + 'static,
{
    let processor = BatchSpanProcessor::builder(exporter, runtime::Tokio)
        .with_batch_config(batch)
        .build();
    let resource = Resource::builder().with_service_name(SERVICE_NAME).build();
    SdkTracerProvider::builder()
        .with_resource(resource)
        .with_span_processor(processor)
        .build()
}

/// The layer that hands every span `filter` enables to `provider`.
///
/// `filter` is reevaluated at every span, so a span the export filter enables reaches
/// `provider` whatever pass another layer's filter ran last on that thread.
#[cfg(feature = "otlp")]
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

/// Always `None`: no exporter exists to install without the `otlp` feature.
#[cfg(not(feature = "otlp"))]
pub(crate) fn layer<S>() -> (Option<impl Layer<S> + Send + Sync>, Export)
where
    S: tracing::Subscriber,
{
    (None::<tracing_subscriber::layer::Identity>, Export {})
}

#[cfg(all(test, feature = "otlp"))]
mod tests {
    use std::sync::{Arc, Mutex};

    use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{BatchConfig, BatchConfigBuilder, SpanData, SpanExporter};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, SubscriberExt as _};
    use tracing_subscriber::{EnvFilter, Layer};

    use super::{Export, export_layer, meter_provider, tracer_provider};

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
        let provider = tracer_provider(exporter.clone(), BatchConfig::default());
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
        let provider = tracer_provider(StalledExporter, batch);
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

    /// A value an instrument records reaches the meter provider the export installed,
    /// under the instrument's own name and unit.
    #[test]
    fn every_recorded_instrument_reaches_the_meter_provider() {
        let runtime = runtime();
        let _entered = runtime.enter();
        let exporter = RecordingMetricExporter::default();
        let export = Export {
            provider: None,
            meters: Some(meter_provider(exporter.clone())),
        };
        export.install_meter();
        crate::traced!(component = "search", operation = "search.request", {});
        crate::metrics().memory.value(4096).record();
        let provider = export.meters.as_ref().expect("the export holds its meters");
        provider
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
        ] {
            assert!(
                received.contains(&(name.to_owned(), unit.to_owned())),
                "{name} in {unit} must be exported: {received:?}"
            );
        }
        export.shutdown();
    }
}
