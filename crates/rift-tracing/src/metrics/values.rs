//! The values a dispatcher's instruments hold in process, and the snapshot that reads them.
//!
//! Each series is one shared point. A gauge is one atomic. A counter and a histogram are
//! split into [`POINT_SHARDS`] shards, each on its own cache line: a counter shard is an
//! atomic, a histogram shard its buckets under a lock of their own, and each thread
//! records into one shard, so threads recording into one series do not contend. A
//! snapshot adds the shards up.
//!
//! The map from instrument and label values to those points lives under one lock, taken
//! when a thread records into a series the first time and when a snapshot reads every
//! series. Each thread keeps the points it recorded into in a cache of its own, keyed by
//! the addresses of the instrument name and label values, so a later record into the same
//! series takes no shared lock. No value is formatted or allocated past the first record
//! of a series.
//!
//! An instrument keeps at most its declared count of label sets, and a label set past that
//! bound records into the instrument's one overflow series, so the memory an instrument
//! holds is fixed by its declaration however many distinct values the code passes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::{Instrument, InstrumentKind, METRIC_LABELS_MAX};
use crate::sampler::ProcessSample;

/// The label values of one series, in declaration order; keys past the declared ones hold
/// the empty string.
pub(crate) type Labels = [&'static str; METRIC_LABELS_MAX];

/// Shards of one counter or histogram series; a thread records into the shard its index
/// names.
const POINT_SHARDS: usize = 8;

/// The index of the next thread to record a value: its shard is this index modulo
/// [`POINT_SHARDS`].
static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

/// Series one thread's cache keeps before it starts over: past it, the next record of a
/// series it dropped finds the point under the shared lock again.
pub(super) const CACHED_POINTS_MAX: usize = 4_096;

/// The identity of the next [`MetricValues`], so a thread's cache never answers a point of
/// another dispatcher's values.
static NEXT_VALUES: AtomicU64 = AtomicU64::new(0);

/// The metric values one dispatcher holds: every instrument's series, and the latest
/// process sample the sampler published.
#[derive(Debug)]
pub(crate) struct MetricValues {
    identity: u64,
    instruments: Mutex<HashMap<&'static str, InstrumentValues>>,
    sample: Mutex<Option<ProcessSample>>,
    #[cfg(feature = "otlp")]
    export: Option<crate::otlp::MetricExport>,
}

impl Default for MetricValues {
    fn default() -> Self {
        Self {
            identity: NEXT_VALUES.fetch_add(1, Ordering::Relaxed),
            instruments: Mutex::default(),
            sample: Mutex::default(),
            #[cfg(feature = "otlp")]
            export: None,
        }
    }
}

impl MetricValues {
    /// Values that also forward every recording to `export`, when it exists.
    #[cfg(feature = "otlp")]
    pub(crate) fn exporting(export: Option<crate::otlp::MetricExport>) -> Self {
        Self {
            export,
            ..Self::default()
        }
    }

    /// Forwards one recording to the OTLP export, outside every lock.
    #[cfg(feature = "otlp")]
    fn forward(&self, instrument: &Instrument, labels: &Labels, overflow: bool, value: f64) {
        if let Some(export) = &self.export {
            export.record(instrument, labels, overflow, value);
        }
    }

    /// Without the `otlp` feature a recording stays in process.
    #[cfg(not(feature = "otlp"))]
    #[expect(
        clippy::unused_self,
        reason = "a build without the otlp feature forwards nowhere"
    )]
    const fn forward(&self, _: &Instrument, _: &Labels, _: bool, _: f64) {}

    /// Adds `value` to the counter series `labels` names.
    pub(crate) fn add(&self, instrument: &Instrument, labels: Labels, value: f64) {
        let overflow = self.update(instrument, labels, |point, shard| point.add(shard, value));
        self.forward(instrument, &labels, overflow, value);
    }

    /// Sets the gauge series `labels` names to `value`.
    pub(crate) fn set(&self, instrument: &Instrument, labels: Labels, value: f64) {
        let overflow = self.update(instrument, labels, |point, _| point.set(value));
        self.forward(instrument, &labels, overflow, value);
    }

    /// Counts `value` into the histogram series `labels` names.
    pub(crate) fn observe(&self, instrument: &Instrument, labels: Labels, value: f64) {
        let overflow = self.update(instrument, labels, |point, shard| {
            point.observe(shard, value);
        });
        self.forward(instrument, &labels, overflow, value);
    }

    /// Runs `update` on the point of the series `labels` names, or on the overflow point
    /// once the instrument holds its bound. Returns whether the value went to the overflow
    /// point.
    ///
    /// The point comes from the thread's cache when the thread recorded into the series
    /// before; otherwise from the shared map, under its lock, and the cache keeps it. A
    /// thread whose cache is already borrowed, by a record made while one runs, or gone, at
    /// thread exit, takes the shared map.
    fn update(
        &self,
        instrument: &Instrument,
        labels: Labels,
        update: impl FnOnce(&Point, usize),
    ) -> bool {
        let key = PointKey::of(instrument.name(), &labels);
        let mut update = Some(update);
        let cached = CACHED_POINTS.try_with(|cache| {
            let Ok(mut cache) = cache.try_borrow_mut() else {
                return None;
            };
            if cache.values != self.identity {
                cache.values = self.identity;
                cache.points.clear();
            }
            if cache.points.len() >= CACHED_POINTS_MAX && !cache.points.contains_key(&key) {
                cache.points.clear();
            }
            let cache = &mut *cache;
            let cached = cache
                .points
                .entry(key)
                .or_insert_with(|| self.shared_point(instrument, labels));
            if let Some(update) = update.take() {
                update(&cached.point, cache.shard);
            }
            Some(cached.overflow)
        });
        if let Ok(Some(overflow)) = cached {
            return overflow;
        }
        let shared = self.shared_point(instrument, labels);
        if let Some(update) = update.take() {
            update(&shared.point, 0);
        }
        shared.overflow
    }

    /// The point of the series `labels` names, created when absent, or the instrument's
    /// overflow point once it holds its bound; read under the shared lock.
    fn shared_point(&self, instrument: &Instrument, labels: Labels) -> CachedPoint {
        let mut instruments = lock(&self.instruments);
        let values = instruments
            .entry(instrument.name())
            .or_insert_with(|| InstrumentValues {
                instrument: *instrument,
                series: HashMap::new(),
                overflow: None,
            });
        let declared = values.instrument;
        if let Some(point) = values.series.get(&labels) {
            return CachedPoint {
                point: Arc::clone(point),
                overflow: false,
            };
        }
        if values.series.len() < declared.series_max() {
            let point = Arc::new(Point::empty(&declared));
            values.series.insert(labels, Arc::clone(&point));
            return CachedPoint {
                point,
                overflow: false,
            };
        }
        let point = values
            .overflow
            .get_or_insert_with(|| Arc::new(Point::empty(&declared)));
        CachedPoint {
            point: Arc::clone(point),
            overflow: true,
        }
    }

    /// Publishes `sample` as the process's latest.
    pub(crate) fn publish_sample(&self, sample: ProcessSample) {
        *lock(&self.sample) = Some(sample);
    }

    /// The latest process sample the sampler published, if any.
    pub(crate) fn latest_sample(&self) -> Option<ProcessSample> {
        *lock(&self.sample)
    }

    /// Every series every instrument holds, read under the shared lock.
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

thread_local! {
    /// The points this thread recorded into, for the values it recorded into last.
    static CACHED_POINTS: RefCell<PointCache> = RefCell::new(PointCache::default());
}

/// The count of points the calling thread's cache holds.
#[cfg(test)]
pub(super) fn cached_points() -> usize {
    CACHED_POINTS.with(|cache| cache.borrow().points.len())
}

/// One thread's shard and its points, all of the values whose identity it names.
struct PointCache {
    shard: usize,
    values: u64,
    points: HashMap<PointKey, CachedPoint, BuildHasherDefault<AddressHasher>>,
}

impl Default for PointCache {
    fn default() -> Self {
        Self {
            shard: NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % POINT_SHARDS,
            values: u64::MAX,
            points: HashMap::default(),
        }
    }
}

/// A point a thread recorded into, and whether it is its instrument's overflow point.
#[derive(Debug)]
struct CachedPoint {
    point: Arc<Point>,
    overflow: bool,
}

/// A series by the addresses and lengths of its instrument name and label values. Two
/// spellings of one name at two addresses are two keys for one point: the shared map,
/// keyed by the text, answers both with it.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PointKey([usize; 2 * (1 + METRIC_LABELS_MAX)]);

impl PointKey {
    fn of(name: &'static str, labels: &Labels) -> Self {
        let mut key = [0; 2 * (1 + METRIC_LABELS_MAX)];
        let (slots, _) = key.as_chunks_mut::<2>();
        for (slot, text) in slots.iter_mut().zip(std::iter::once(&name).chain(labels)) {
            *slot = [text.as_ptr() as usize, text.len()];
        }
        Self(key)
    }
}

/// A multiply-and-rotate hasher for the address words of a [`PointKey`]: the words are
/// already distinct, so the hash only spreads them.
#[derive(Default)]
struct AddressHasher(u64);

impl AddressHasher {
    const MULTIPLIER: u64 = 0x517c_c1b7_2722_0a95;

    const fn mix(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(Self::MULTIPLIER);
    }
}

impl Hasher for AddressHasher {
    fn write(&mut self, bytes: &[u8]) {
        let (words, remainder) = bytes.as_chunks::<8>();
        for word in words {
            self.mix(u64::from_ne_bytes(*word));
        }
        for byte in remainder {
            self.mix(u64::from(*byte));
        }
    }

    fn write_usize(&mut self, word: usize) {
        self.mix(word as u64);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// One instrument's series, keyed by their label values, and its overflow series.
#[derive(Debug)]
struct InstrumentValues {
    instrument: Instrument,
    series: HashMap<Labels, Arc<Point>>,
    overflow: Option<Arc<Point>>,
}

/// One shard of a point, alone on its cache line.
#[derive(Debug, Default)]
#[repr(align(128))]
struct Shard<Value>(Value);

/// What one series holds.
#[derive(Debug)]
enum Point {
    /// A counter's sum, per shard, as the bits of an `f64`.
    Sum(Box<[Shard<AtomicU64>; POINT_SHARDS]>),
    /// A gauge's latest value, as the bits of an `f64`.
    Last(AtomicU64),
    /// A histogram's counts per shard, with the boundaries its declaration fixed.
    Buckets(Box<[Shard<Mutex<Buckets>>; POINT_SHARDS]>, &'static [f64]),
}

impl Point {
    /// The point a series of `instrument` starts from.
    fn empty(instrument: &Instrument) -> Self {
        let zero = 0.0_f64.to_bits();
        match instrument.kind() {
            InstrumentKind::Counter => Self::Sum(Box::new(std::array::from_fn(|_| {
                Shard(AtomicU64::new(zero))
            }))),
            InstrumentKind::Gauge => Self::Last(AtomicU64::new(zero)),
            InstrumentKind::Histogram => {
                let boundaries = instrument.boundaries();
                Self::Buckets(
                    Box::new(std::array::from_fn(|_| {
                        Shard(Mutex::new(Buckets::empty(boundaries.len())))
                    })),
                    boundaries,
                )
            }
        }
    }

    /// Adds `value` to the counter's shard `shard`.
    fn add(&self, shard: usize, value: f64) {
        if let Self::Sum(shards) = self
            && let Some(Shard(sum)) = shards.get(shard)
        {
            let _ = sum.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
                Some((f64::from_bits(bits) + value).to_bits())
            });
        }
    }

    /// Sets a gauge's latest value.
    fn set(&self, value: f64) {
        if let Self::Last(last) = self {
            last.store(value.to_bits(), Ordering::Relaxed);
        }
    }

    /// Counts `value` into the bucket, count, and sum of the histogram's shard `shard`,
    /// together.
    fn observe(&self, shard: usize, value: f64) {
        if let Self::Buckets(shards, boundaries) = self
            && let Some(Shard(buckets)) = shards.get(shard)
        {
            let bucket = boundaries.partition_point(|upper| *upper < value);
            let mut buckets = lock(buckets);
            buckets.count += 1;
            buckets.sum += value;
            if let Some(count) = buckets.counts.get_mut(bucket) {
                *count += 1;
            }
        }
    }

    /// The point as a snapshot reads it, its shards added up.
    fn read(&self, instrument: &Instrument) -> SeriesValue {
        match self {
            Self::Sum(shards) => SeriesValue::Sum(
                shards
                    .iter()
                    .map(|Shard(sum)| f64::from_bits(sum.load(Ordering::Relaxed)))
                    .sum(),
            ),
            Self::Last(last) => SeriesValue::Last(f64::from_bits(last.load(Ordering::Relaxed))),
            Self::Buckets(shards, boundaries) => {
                let mut total = Buckets::empty(boundaries.len());
                for Shard(buckets) in shards.iter() {
                    let buckets = lock(buckets);
                    total.count += buckets.count;
                    total.sum += buckets.sum;
                    for (sum, count) in total.counts.iter_mut().zip(&buckets.counts) {
                        *sum += count;
                    }
                }
                SeriesValue::Buckets {
                    count: total.count,
                    sum: total.sum,
                    counts: instrument
                        .boundaries()
                        .iter()
                        .copied()
                        .map(Some)
                        .chain([None])
                        .zip(total.counts)
                        .collect(),
                }
            }
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

impl Buckets {
    /// No values, over `boundaries` bucket boundaries.
    fn empty(boundaries: usize) -> Self {
        Self {
            count: 0,
            sum: 0.0,
            counts: vec![0; boundaries + 1],
        }
    }
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
