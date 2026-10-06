//! What a test reads back from the instruments: the OpenTelemetry SDK's own export of the
//! process's meter, kept in memory.
//!
//! The first recorder a process installs builds one meter provider whose reader exports
//! into the SDK's `InMemoryMetricExporter`, with cumulative temporality, and installs its
//! meter. The provider bounds each instrument at [`CARDINALITY_LIMIT`](crate::CARDINALITY_LIMIT)
//! series, as the OTLP export's provider does. A read flushes the provider and takes the
//! newest export: every series the SDK aggregated since the meter was installed. Nothing here aggregates; a series is one
//! exported point, its labels and value as the SDK reported them.

use std::fmt;
use std::sync::OnceLock;

use opentelemetry::metrics::MeterProvider as _;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

/// The meter provider every recorder of the process reads, and the exporter it exports
/// into.
struct RecorderMeters {
    provider: SdkMeterProvider,
    exporter: InMemoryMetricExporter,
}

static RECORDER_METERS: OnceLock<RecorderMeters> = OnceLock::new();

/// Installs the recorders' meter provider unless the process already has a meter: a meter
/// installed first, such as the `otlp` export's, stays and the reads see none of its
/// values.
pub(crate) fn install() {
    let _ = meters();
}

fn meters() -> &'static RecorderMeters {
    RECORDER_METERS.get_or_init(|| {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .with_view(crate::metrics::cardinality_view(
                crate::metrics::CARDINALITY_LIMIT,
            ))
            .build();
        crate::metrics::install_meter(provider.meter_with_scope(crate::metrics::scope()));
        RecorderMeters { provider, exporter }
    })
}

/// Every series the SDK exports now; empty when nothing was recorded.
pub(crate) fn snapshot() -> MetricSnapshot {
    let meters = meters();
    meters.exporter.reset();
    if meters.provider.force_flush().is_err() {
        return MetricSnapshot::default();
    }
    meters
        .exporter
        .get_finished_metrics()
        .ok()
        .and_then(|exported| exported.last().map(MetricSnapshot::of))
        .unwrap_or_default()
}

/// Every series one export of the process's instruments holds, ordered by instrument
/// name, then labels.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetricSnapshot {
    series: Vec<MetricSeries>,
}

impl MetricSnapshot {
    fn of(exported: &ResourceMetrics) -> Self {
        let mut series = Vec::new();
        for scope in exported.scope_metrics() {
            for metric in scope.metrics() {
                let mut push = |labels, value| {
                    series.push(MetricSeries {
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
            (&first.name, &first.labels).cmp(&(&second.name, &second.labels))
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
    name: String,
    unit: String,
    labels: Vec<(String, String)>,
    value: SeriesValue,
}

impl MetricSeries {
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
/// the instrument's name, its labels as `key=value`, then `value=` for a sum or a gauge, or
/// `count=`, `sum=`, and the nonempty buckets as `<=bound:count`, the last `>bound:count`,
/// for a histogram, then `unit=` when the instrument names one.
impl fmt::Display for MetricSeries {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}   ", self.name)?;
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
