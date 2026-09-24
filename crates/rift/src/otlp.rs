//! Optional OTLP export of Rift's `tracing` spans, behind the `otlp` cargo feature.
//!
//! Off by default, so a release binary built without `--features otlp` carries no
//! OpenTelemetry export stack. Compiled in, the process still exports nothing until an
//! operator sets `OTEL_EXPORTER_OTLP_ENDPOINT` - a local Jaeger started with `just
//! trace-collector`, or any other OTLP/HTTP receiver.

#[cfg(feature = "otlp")]
use opentelemetry::trace::TracerProvider as _;
#[cfg(feature = "otlp")]
use opentelemetry_otlp::{Protocol, SpanExporter, WithExportConfig as _};
#[cfg(feature = "otlp")]
use opentelemetry_sdk::Resource;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::runtime;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::trace::SdkTracerProvider;
#[cfg(feature = "otlp")]
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;

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
const DEFAULT_OTLP_FILTER: &str = "rift=info,rift_mcp=info,rift_server=info,rift_index=info";

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
    let processor = BatchSpanProcessor::builder(exporter, runtime::Tokio).build();
    let resource = Resource::builder().with_service_name(SERVICE_NAME).build();
    let provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_span_processor(processor)
        .build();
    let tracer = provider.tracer(SERVICE_NAME);
    let filter = std::env::var(RIFT_OTLP_FILTER_VAR)
        .ok()
        .and_then(|value| tracing_subscriber::EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new(DEFAULT_OTLP_FILTER));
    let layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_filter(filter);
    (
        Some(layer),
        Export {
            provider: Some(provider),
        },
    )
}

/// Always `None`: no exporter exists to install without the `otlp` feature.
#[cfg(not(feature = "otlp"))]
pub(crate) fn layer<S>() -> (Option<impl Layer<S> + Send + Sync>, Export)
where
    S: tracing::Subscriber,
{
    (None::<tracing_subscriber::layer::Identity>, Export {})
}
