//! Optional OTLP export of Rift's `tracing` spans, behind the `otlp` cargo feature.
//!
//! Off by default, so a release binary built without `--features otlp` carries no
//! OpenTelemetry export stack. Compiled in, the process still exports nothing until an
//! operator sets `OTEL_EXPORTER_OTLP_ENDPOINT` - the in-memory collector `just
//! trace-collector` runs, or any other OTLP/HTTP receiver.

#[cfg(feature = "otlp")]
use opentelemetry::trace::TracerProvider as _;
#[cfg(feature = "otlp")]
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig as _};
#[cfg(feature = "otlp")]
use opentelemetry_sdk::Resource;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::runtime;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::trace::{BatchConfig, SdkTracerProvider};

use tracing_subscriber::Layer;
#[cfg(feature = "otlp")]
use tracing_subscriber::registry::LookupSpan;

/// The `service.name` resource attribute every exported span carries.
#[cfg(feature = "otlp")]
const SERVICE_NAME: &str = "rift";
/// Overrides the export layer's own filter; unset, [`DEFAULT_OTLP_FILTER`] applies.
#[cfg(feature = "otlp")]
const RIFT_OTLP_FILTER_VAR: &str = "RIFT_OTLP_FILTER";
/// Keeps Rift's own crates - the ones `traced!` and `traced_async!` instrument - at
/// info; a dependency's own spans stay out unless the operator names it.
#[cfg(feature = "otlp")]
const DEFAULT_OTLP_FILTER: &str =
    "rift=info,rift_mcp=info,rift_server=info,rift_index=info,rift_analysis=info";

/// The installed exporter's tracer provider, held so the caller can flush and shut it
/// down before the process exits.
///
/// Holds nothing when the `otlp` feature is not compiled in, or when no collector
/// endpoint was configured; [`Export::shutdown`] is then a no-op.
pub(crate) struct Export {
    #[cfg(feature = "otlp")]
    provider: Option<SdkTracerProvider>,
}

impl Export {
    /// Flushes buffered spans and shuts the tracer provider down.
    ///
    /// A collector that is unreachable at shutdown is not this process's failure: the
    /// server has already finished serving, and losing the last batch of spans must not
    /// turn a clean run into a nonzero exit status.
    pub(crate) fn shutdown(self) {
        #[cfg(feature = "otlp")]
        if let Some(provider) = self.provider
            && let Err(error) = provider.shutdown()
        {
            eprintln!("rift: warning: otlp shutdown failed: {error}");
        }
    }
}

/// Installs an OTLP/HTTP export layer when `OTEL_EXPORTER_OTLP_ENDPOINT` names a
/// collector, `None` otherwise.
///
/// Generic in the subscriber `S` because `tracing_subscriber::registry().with(a).with(b)`
/// changes the concrete subscriber type at every `.with()` call; a layer boxed as
/// `dyn Layer<Registry>` only satisfies the first one in the chain. Returning `impl
/// Layer<S>` lets this layer adapt to wherever `initialize_tracing` appends it instead.
///
/// The exporter posts protobuf-encoded OTLP over the async `reqwest` client Rift already
/// depends on, batched by a Tokio-driven [`BatchSpanProcessor`]: `opentelemetry_sdk`'s
/// default batch processor exports on a dedicated `std::thread` through
/// `futures_executor::block_on`, which has no Tokio reactor to poll an async HTTP client
/// on, and Rift never uses `reqwest::blocking`. Checking the endpoint variable before
/// building anything keeps the feature from silently dialing OTLP's default
/// `http://localhost:4318` the moment it is compiled in.
#[cfg(feature = "otlp")]
pub(crate) fn layer<S>() -> (Option<impl Layer<S> + Send + Sync>, Export)
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span> + Send + Sync,
{
    if std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_none() {
        return (None, Export { provider: None });
    }
    let exporter = match SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .build()
    {
        Ok(exporter) => exporter,
        Err(error) => {
            eprintln!("rift: warning: otlp exporter did not build: {error}");
            return (None, Export { provider: None });
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
        },
    )
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
        .with_filter(crate::reevaluated(filter))
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

    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{BatchConfig, SpanData, SpanExporter};
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::{EnvFilter, Layer as _};

    use super::{export_layer, tracer_provider};

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
                rift_core::traced!(component = "search", operation = "search.request", {});
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
}
