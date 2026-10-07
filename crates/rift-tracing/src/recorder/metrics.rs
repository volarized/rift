//! What a test reads back from the instruments: the OpenTelemetry SDK's current collection
//! of the process's meter.
//!
//! The first recorder a process installs builds one meter provider and retains its reader.
//! A read collects once. With an OTLP exporter, that collection also supplies the points
//! for export. The provider bounds each instrument at
//! [`CARDINALITY_LIMIT`](crate::CARDINALITY_LIMIT) series. Nothing here aggregates; a series
//! is one point, its labels and value as the SDK reported them.

use std::fmt;
use std::sync::{Arc, OnceLock};

use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::reader::MetricReader;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

pub(crate) type MetricExport =
    Arc<dyn Fn(ResourceMetrics) -> Result<(), crate::otlp::ExportShutdownError> + Send + Sync>;

/// The meter provider and reader every recorder of the process reads.
struct RecorderMeters {
    reader: Arc<dyn MetricReader>,
    exporter: Option<InMemoryMetricExporter>,
    metric_export: Option<MetricExport>,
}

static RECORDER_METERS: OnceLock<RecorderMeters> = OnceLock::new();

/// Installs the recorder's meter provider and reader when configured.
pub(crate) fn install(
    provider: Option<SdkMeterProvider>,
    reader: Option<Arc<dyn MetricReader>>,
    metric_export: Option<MetricExport>,
) {
    let _ = RECORDER_METERS.get_or_init(|| {
        let (provider, reader, exporter, metric_export) =
            if let (Some(provider), Some(reader)) = (provider, reader) {
                (provider, reader, None, metric_export)
            } else {
                let (provider, reader, exporter) = local_provider();
                (provider, reader, exporter, None)
            };
        crate::metrics::install_meter(provider.clone());
        RecorderMeters {
            reader,
            exporter,
            metric_export,
        }
    });
}

fn meters() -> &'static RecorderMeters {
    RECORDER_METERS.get_or_init(|| {
        let (provider, reader, exporter) = local_provider();
        crate::metrics::install_meter(provider.clone());
        RecorderMeters {
            reader,
            exporter,
            metric_export: None,
        }
    })
}

fn local_provider() -> (
    SdkMeterProvider,
    Arc<dyn MetricReader>,
    Option<InMemoryMetricExporter>,
) {
    let exporter = InMemoryMetricExporter::default();
    let reader = PeriodicReader::builder(exporter.clone()).build();
    let reader_handle: Arc<dyn MetricReader> = Arc::new(reader.clone());
    let provider = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_view(crate::metrics::cardinality_view(
            crate::metrics::CARDINALITY_LIMIT,
        ))
        .build();
    (provider, reader_handle, Some(exporter))
}

/// Every series in the current SDK collection; empty when nothing is recorded.
pub(crate) fn snapshot() -> MetricSnapshot {
    let meters = meters();
    let mut exported = ResourceMetrics::default();
    if meters.reader.collect(&mut exported).is_err() {
        if let Some(exporter) = &meters.exporter {
            exporter.reset();
        }
        return MetricSnapshot::default();
    }
    if let Some(exporter) = &meters.exporter {
        exporter.reset();
    }
    let snapshot = MetricSnapshot::of(&exported);
    if let Some(export) = &meters.metric_export {
        let _ = export(exported);
    }
    snapshot
}

/// Every series one export of the process's instruments holds, ordered by instrument
/// name, then labels, then instrumentation scope.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetricSnapshot {
    series: Vec<MetricSeries>,
}

impl MetricSnapshot {
    fn of(exported: &ResourceMetrics) -> Self {
        let mut series = Vec::new();
        for scope in exported.scope_metrics() {
            let scope_name = scope.scope().name();
            let scope_version = scope.scope().version().unwrap_or_default();
            for metric in scope.metrics() {
                let mut push = |labels, value| {
                    series.push(MetricSeries {
                        scope_name: scope_name.to_owned(),
                        scope_version: scope_version.to_owned(),
                        name: metric.name().to_owned(),
                        unit: metric.unit().to_owned(),
                        labels,
                        value,
                    });
                };
                match metric.data() {
                    AggregatedMetrics::F64(data) => points(data, &mut push),
                    AggregatedMetrics::U64(data) => points(data, &mut push),
                    AggregatedMetrics::I64(data) => points(data, &mut push),
                }
            }
        }
        series.sort_by(|first, second| {
            (&first.name, &first.labels, &first.scope_name).cmp(&(
                &second.name,
                &second.labels,
                &second.scope_name,
            ))
        });
        Self { series }
    }

    /// Every series, in order.
    #[must_use]
    pub fn series(&self) -> &[MetricSeries] {
        &self.series
    }

    /// The series of the instrument `name` whose labels are exactly `labels`, as key and
    /// value pairs in any order; a label recorded empty is never exported.
    #[must_use]
    pub fn find(&self, name: &str, labels: &[(&str, &str)]) -> Option<&MetricSeries> {
        self.series.iter().find(|series| {
            series.name == name
                && series.labels.len() == labels.len()
                && labels.iter().all(|(key, value)| {
                    series
                        .labels
                        .iter()
                        .any(|(held_key, held_value)| held_key == key && held_value == value)
                })
        })
    }
}

/// A value type the SDK exports, read as a float.
trait Exported: Copy {
    fn float(self) -> f64;
}

impl Exported for f64 {
    fn float(self) -> f64 {
        self
    }
}

impl Exported for u64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a test's count stays far below 2^53"
    )]
    fn float(self) -> f64 {
        self as f64
    }
}

impl Exported for i64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a test's value stays far below 2^53"
    )]
    fn float(self) -> f64 {
        self as f64
    }
}

/// Hands every exported point of `data` to `push`, with its labels sorted by key.
fn points<Value: Exported>(
    data: &MetricData<Value>,
    push: &mut impl FnMut(Vec<(String, String)>, SeriesValue),
) {
    let labels = |attributes: &mut dyn Iterator<Item = &opentelemetry::KeyValue>| {
        let mut labels: Vec<(String, String)> = attributes
            .map(|pair| {
                (
                    pair.key.as_str().to_owned(),
                    pair.value.as_str().into_owned(),
                )
            })
            .collect();
        labels.sort();
        labels
    };
    match data {
        MetricData::Sum(sum) => {
            for point in sum.data_points() {
                push(
                    labels(&mut point.attributes()),
                    SeriesValue::Sum(point.value().float()),
                );
            }
        }
        MetricData::Gauge(gauge) => {
            for point in gauge.data_points() {
                push(
                    labels(&mut point.attributes()),
                    SeriesValue::Last(point.value().float()),
                );
            }
        }
        MetricData::Histogram(histogram) => {
            for point in histogram.data_points() {
                push(
                    labels(&mut point.attributes()),
                    SeriesValue::Buckets {
                        count: point.count(),
                        sum: point.sum().float(),
                        counts: point
                            .bounds()
                            .map(Some)
                            .chain([None])
                            .zip(point.bucket_counts())
                            .collect(),
                    },
                );
            }
        }
        MetricData::ExponentialHistogram(_) => {}
    }
}

/// One exported series: an instrument, the labels that select it, and what it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricSeries {
    scope_name: String,
    scope_version: String,
    name: String,
    unit: String,
    labels: Vec<(String, String)>,
    value: SeriesValue,
}

impl MetricSeries {
    /// The name of the instrumentation scope the series was exported under: the Cargo
    /// package name of the crate that emits it.
    #[must_use]
    pub fn scope_name(&self) -> &str {
        &self.scope_name
    }

    /// The version of the instrumentation scope: the emitting crate's Cargo version.
    #[must_use]
    pub fn scope_version(&self) -> &str {
        &self.scope_version
    }

    /// The instrument's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The instrument's unit, in the OpenTelemetry spelling.
    #[must_use]
    pub fn unit(&self) -> &str {
        &self.unit
    }

    /// The label keys and values that select the series, sorted by key.
    #[must_use]
    pub fn labels(&self) -> Vec<(&str, &str)> {
        self.labels
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect()
    }

    /// What the series holds.
    #[must_use]
    pub const fn value(&self) -> &SeriesValue {
        &self.value
    }
}

/// One line per series, in the layout the developer collector prints a metric point
/// (`dev/src/rift_dev/trace.py`, `MetricPoint.line`) without its time and sending process:
/// the instrument's name, its instrumentation scope as `otel.scope.name=` and
/// `otel.scope.version=`, its labels as `key=value`, then `value=` for a sum or a gauge, or
/// `count=`, `sum=`, and the nonempty buckets as `<=bound:count`, the last `>bound:count`,
/// for a histogram, then `unit=` when the instrument names one.
impl fmt::Display for MetricSeries {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}   otel.scope.name={} otel.scope.version={}  ",
            self.name, self.scope_name, self.scope_version
        )?;
        for (key, value) in &self.labels {
            write!(formatter, "{key}={value} ")?;
        }
        if !self.labels.is_empty() {
            formatter.write_str(" ")?;
        }
        match &self.value {
            SeriesValue::Sum(value) | SeriesValue::Last(value) => {
                write!(formatter, "value={value}")?;
            }
            SeriesValue::Buckets { count, sum, counts } => {
                write!(formatter, "count={count} sum={sum}")?;
                let mut separator = " buckets=";
                let last_bound = counts.iter().rev().find_map(|(bound, _)| *bound);
                for (bound, bucket) in counts.iter().filter(|(_, bucket)| *bucket > 0) {
                    match (bound, last_bound) {
                        (Some(bound), _) => write!(formatter, "{separator}<={bound}:{bucket}")?,
                        (None, Some(last)) => write!(formatter, "{separator}>{last}:{bucket}")?,
                        (None, None) => write!(formatter, "{separator}>-inf:{bucket}")?,
                    }
                    separator = ",";
                }
            }
        }
        if !self.unit.is_empty() {
            write!(formatter, " unit={}", self.unit)?;
        }
        Ok(())
    }
}

/// What one exported series holds.
#[derive(Clone, Debug, PartialEq)]
pub enum SeriesValue {
    /// A counter's sum since the meter was installed.
    Sum(f64),
    /// A gauge's latest value.
    Last(f64),
    /// A histogram's count and sum, and the count of each bucket under its upper boundary;
    /// the last bucket, past every boundary, has none.
    Buckets {
        /// Values recorded.
        count: u64,
        /// Their sum, in the instrument's unit.
        sum: f64,
        /// Each bucket's upper boundary and count.
        counts: Vec<(Option<f64>, u64)>,
    },
}
