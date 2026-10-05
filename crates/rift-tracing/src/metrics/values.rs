//! The values a dispatcher's instruments hold in process, and the snapshot that reads them.
//!
//! Every series lives under one lock, held for one lookup and one update; no value is
//! formatted or allocated past the first record of a series. An instrument keeps at most
//! its declared count of label sets, and a label set past that bound records into the
//! instrument's one overflow series, so the memory an instrument holds is fixed by its
//! declaration however many distinct values the code passes.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{Instrument, InstrumentKind, METRIC_LABELS_MAX};
use crate::sampler::ProcessSample;

/// The label values of one series, in declaration order; keys past the declared ones hold
/// the empty string.
pub(crate) type Labels = [&'static str; METRIC_LABELS_MAX];

/// The metric values one dispatcher holds: every instrument's series, and the latest
/// process sample the sampler published.
#[derive(Debug, Default)]
pub(crate) struct MetricValues {
    instruments: Mutex<HashMap<&'static str, InstrumentValues>>,
    sample: Mutex<Option<ProcessSample>>,
}

impl MetricValues {
    /// Adds `value` to the counter series `labels` names.
    pub(crate) fn add(&self, instrument: &Instrument, labels: Labels, value: f64) {
        self.update(instrument, labels, |point| {
            if let Point::Sum(sum) = point {
                *sum += value;
            }
        });
    }

    /// Sets the gauge series `labels` names to `value`.
    pub(crate) fn set(&self, instrument: &Instrument, labels: Labels, value: f64) {
        self.update(instrument, labels, |point| {
            if let Point::Last(last) = point {
                *last = value;
            }
        });
    }

    /// Counts `value` into the histogram series `labels` names.
    pub(crate) fn observe(&self, instrument: &Instrument, labels: Labels, value: f64) {
        let boundaries = instrument.boundaries();
        self.update(instrument, labels, |point| {
            if let Point::Buckets(buckets) = point {
                buckets.count += 1;
                buckets.sum += value;
                let bucket = boundaries.partition_point(|upper| *upper < value);
                buckets.counts[bucket] += 1;
            }
        });
    }

    /// Runs `update` on the point of the series `labels` names, creating it, or on the
    /// overflow point once the instrument holds its bound. Returns whether the value went
    /// to the overflow point.
    fn update(
        &self,
        instrument: &Instrument,
        labels: Labels,
        update: impl FnOnce(&mut Point),
    ) -> bool {
        let mut instruments = lock(&self.instruments);
        let values = instruments
            .entry(instrument.name())
            .or_insert_with(|| InstrumentValues {
                instrument: *instrument,
                series: HashMap::new(),
                overflow: None,
            });
        let kind = values.instrument.kind();
        let boundaries = values.instrument.boundaries().len();
        if let Some(point) = values.series.get_mut(&labels) {
            update(point);
            return false;
        }
        if values.series.len() < values.instrument.series_max() {
            update(
                values
                    .series
                    .entry(labels)
                    .or_insert(Point::empty(kind, boundaries)),
            );
            return false;
        }
        update(
            values
                .overflow
                .get_or_insert_with(|| Point::empty(kind, boundaries)),
        );
        true
    }

    /// Publishes `sample` as the process's latest.
    pub(crate) fn publish_sample(&self, sample: ProcessSample) {
        *lock(&self.sample) = Some(sample);
    }

    /// The latest process sample the sampler published, if any.
    pub(crate) fn latest_sample(&self) -> Option<ProcessSample> {
        *lock(&self.sample)
    }

    /// Every series every instrument holds, read under one lock.
    pub(crate) fn snapshot(&self) -> MetricSnapshot {
        let instruments = lock(&self.instruments);
        let mut series = Vec::new();
        for values in instruments.values() {
            let instrument = values.instrument;
            for (labels, point) in &values.series {
                let keys = instrument.label_keys();
                series.push(MetricSeries {
                    instrument,
                    labels: keys
                        .iter()
                        .zip(labels)
                        .filter(|(_, value)| !value.is_empty())
                        .map(|(key, value)| (*key, *value))
                        .collect(),
                    overflow: false,
                    value: point.read(&instrument),
                });
            }
            if let Some(point) = &values.overflow {
                series.push(MetricSeries {
                    instrument,
                    labels: Vec::new(),
                    overflow: true,
                    value: point.read(&instrument),
                });
            }
        }
        series.sort_by(|left, right| {
            (left.name(), left.overflow, &left.labels).cmp(&(
                right.name(),
                right.overflow,
                &right.labels,
            ))
        });
        MetricSnapshot {
            series,
            sampled_at_ms: self.latest_sample().map(|sample| sample.recorded_at_ms()),
        }
    }
}

/// Locks `mutex`, taking the value a panicking holder left: every update leaves a point
/// whole, so a panic between two updates loses nothing a reader relies on.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One instrument's series, keyed by their label values, and its overflow series.
#[derive(Debug)]
struct InstrumentValues {
    instrument: Instrument,
    series: HashMap<Labels, Point>,
    overflow: Option<Point>,
}

/// What one series holds.
#[derive(Debug)]
enum Point {
    /// A counter's sum.
    Sum(f64),
    /// A gauge's latest value.
    Last(f64),
    /// A histogram's counts.
    Buckets(Buckets),
}

impl Point {
    /// The point a series of `kind` starts from, with `boundaries` bucket boundaries.
    fn empty(kind: InstrumentKind, boundaries: usize) -> Self {
        match kind {
            InstrumentKind::Counter => Self::Sum(0.0),
            InstrumentKind::Gauge => Self::Last(0.0),
            InstrumentKind::Histogram => Self::Buckets(Buckets {
                count: 0,
                sum: 0.0,
                counts: vec![0; boundaries + 1],
            }),
        }
    }

    /// The point as a snapshot reads it.
    fn read(&self, instrument: &Instrument) -> SeriesValue {
        match self {
            Self::Sum(sum) => SeriesValue::Sum(*sum),
            Self::Last(last) => SeriesValue::Last(*last),
            Self::Buckets(buckets) => SeriesValue::Buckets {
                count: buckets.count,
                sum: buckets.sum,
                counts: instrument
                    .boundaries()
                    .iter()
                    .copied()
                    .map(Some)
                    .chain([None])
                    .zip(buckets.counts.iter().copied())
                    .collect(),
            },
        }
    }
}

/// A histogram series' counts: one per bucket, the last past every boundary.
#[derive(Debug)]
struct Buckets {
    count: u64,
    sum: f64,
    counts: Vec<u64>,
}

/// Every series a dispatcher's instruments hold at one moment, ordered by instrument name,
/// then label values, with an instrument's overflow series last.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MetricSnapshot {
    series: Vec<MetricSeries>,
    sampled_at_ms: Option<i64>,
}

impl MetricSnapshot {
    /// When the process sample the process gauges hold was taken, in milliseconds since
    /// the Unix epoch; absent until the sampler published one.
    #[must_use]
    pub const fn sampled_at_ms(&self) -> Option<i64> {
        self.sampled_at_ms
    }

    /// Every series, in order.
    #[must_use]
    pub fn series(&self) -> &[MetricSeries] {
        &self.series
    }

    /// The series of the instrument `name` whose label values are `labels`, as key and
    /// value pairs in declaration order, leaving out keys recorded empty.
    #[must_use]
    pub fn find(&self, name: &str, labels: &[(&str, &str)]) -> Option<&MetricSeries> {
        self.series.iter().find(|series| {
            series.name() == name
                && !series.overflow
                && series.labels.len() == labels.len()
                && series
                    .labels
                    .iter()
                    .zip(labels)
                    .all(|(held, asked)| held.0 == asked.0 && held.1 == asked.1)
        })
    }
}

/// One series: an instrument, the label values that select it, and what it holds.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricSeries {
    instrument: Instrument,
    labels: Vec<(&'static str, &'static str)>,
    overflow: bool,
    value: SeriesValue,
}

impl MetricSeries {
    /// The instrument's name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.instrument.name()
    }

    /// The instrument this series belongs to.
    #[must_use]
    pub const fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    /// The label keys and values that select the series, in declaration order; a key
    /// recorded empty is left out.
    #[must_use]
    pub fn labels(&self) -> &[(&'static str, &'static str)] {
        &self.labels
    }

    /// Whether this is the series that holds every label set past the instrument's bound.
    #[must_use]
    pub const fn is_overflow(&self) -> bool {
        self.overflow
    }

    /// What the series holds.
    #[must_use]
    pub const fn value(&self) -> &SeriesValue {
        &self.value
    }
}

/// What one series holds when a snapshot reads it.
#[derive(Clone, Debug, PartialEq)]
pub enum SeriesValue {
    /// A counter's sum since the dispatcher started.
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
