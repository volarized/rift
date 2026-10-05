//! Typed metric instruments over the OpenTelemetry metrics API: counters, gauges, and
//! histograms a declaration fixes once.
//!
//! A declaration is a `static` naming an instrument, its unit, and the label keys it
//! accepts. A caller records into the declared handle; it never registers an instrument or
//! builds a label map. Label values are `&'static str`, so a value comes from a closed set
//! the code spells out, and a path, a query, or an error message cannot become a label.
//!
//! A recording hands the value and its labels to the OpenTelemetry instrument the
//! declaration holds, built from the process's meter on the first recording after one was
//! installed. The OpenTelemetry SDK aggregates, bounds the series, and exports; this
//! module holds no value. The process installs at most one meter: the OTLP export when an
//! endpoint is configured, or a test's `ScopedRecorder`. Before that, and in a process that
//! installs none, a recording reads two atomics and records nothing.
//!
//! A quantity its owner holds, such as a file's size or a queue's length, is an
//! [`ObservableUpDownCounter`]: the owner registers a read that runs each time the SDK's
//! reader collects, on the reader's task, and the read leaves when the owner drops the
//! [`ObservationGuard`] it got back. The installed SDK keeps every callback for the meter
//! provider's life and removes none, so each instrument registers one callback, once, and
//! that callback runs the instrument's own list of at most [`OBSERVATIONS_MAX`] reads.
//!
//! A meter obtained from `opentelemetry::global` before a provider is set stays a no-op for
//! good, so the meter is never taken from there: the installer hands it over once, and
//! each declaration builds its instrument from it lazily.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{AsyncInstrument, Meter};

use crate::capture::span_failure;
use crate::measurement::monotonic_now;

/// Upper bucket boundaries, in seconds, of a duration histogram whose declaration names no
/// others: the boundaries the OpenTelemetry HTTP semantic conventions advise for durations.
pub const DURATION_BOUNDARIES_SECONDS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// The meter every declaration builds its instrument from, installed at most once.
static METER: OnceLock<Meter> = OnceLock::new();

/// The instrumentation scope of every Rift instrument: `rift-tracing` and its version,
/// because every instrument is declared through this crate.
pub(crate) fn scope() -> opentelemetry::InstrumentationScope {
    opentelemetry::InstrumentationScope::builder(env!("CARGO_PKG_NAME"))
        .with_version(env!("CARGO_PKG_VERSION"))
        .build()
}

/// Series one instrument aggregates, at most: the explicit cardinality bound every Rift
/// instrument carries. A measurement that would add a series past it joins the series the
/// SDK labels `otel.metric.overflow` = `true`.
///
/// The figure is the OpenTelemetry SDK's own default, 2,000, stated here so the bound is
/// Rift's declaration rather than the SDK's choice.
pub const CARDINALITY_LIMIT: usize = 2_000;

/// The view every meter provider Rift builds applies to each instrument: the stream the
/// instrument names, bounded at `limit` series.
///
/// The stream keeps the instrument's name, unit, and histogram boundaries; the SDK fills
/// each from the instrument when the view leaves it unset.
pub(crate) fn cardinality_view(
    limit: usize,
) -> impl Fn(&opentelemetry_sdk::metrics::Instrument) -> Option<opentelemetry_sdk::metrics::Stream>
+ Send
+ Sync
+ 'static {
    move |_instrument| {
        opentelemetry_sdk::metrics::Stream::builder()
            .with_cardinality_limit(limit)
            .build()
            .ok()
    }
}

/// Makes `meter` the one every declaration builds its instrument from; a meter installed
/// before stays, and `meter` is dropped.
///
/// It then builds the counters a `tracing` layer records into, which record through
/// [`Counter::add_built`] alone: `log.queue.dropped` and `operation.untracked`.
pub(crate) fn install_meter(meter: Meter) {
    let _ = METER.set(meter);
    crate::capture::LOG_QUEUE_DROPPED.build();
    crate::flight::OPERATION_UNTRACKED.build();
}

/// Whether a meter is installed, so a recording reaches an instrument.
pub(crate) fn meter_installed() -> bool {
    METER.get().is_some()
}

/// Runs `register` against the installed meter; answers whether a meter was installed.
pub(crate) fn with_meter(register: impl FnOnce(&Meter)) -> bool {
    METER.get().map(register).is_some()
}

/// The instrument `cell` holds, built by `build` from the installed meter on first use;
/// `None` while no meter is installed, so a later install still builds it.
fn built<Handle>(cell: &OnceLock<Handle>, build: impl FnOnce(&Meter) -> Handle) -> Option<&Handle> {
    if let Some(handle) = cell.get() {
        return Some(handle);
    }
    let meter = METER.get()?;
    Some(cell.get_or_init(|| build(meter)))
}

/// The attributes the label `values` name, in declaration order, and how many lead the
/// array: a value recorded empty names no attribute, so a success carries no `error.type`.
fn attributes<const LABELS: usize>(
    keys: &'static [&'static str; LABELS],
    values: [&'static str; LABELS],
) -> ([KeyValue; LABELS], usize) {
    let mut present = keys
        .iter()
        .zip(values)
        .filter(|(_, value)| !value.is_empty());
    let mut count = 0;
    let attributes = std::array::from_fn(|_| match present.next() {
        Some((key, value)) => {
            count += 1;
            KeyValue::new(*key, value)
        }
        None => KeyValue::new("", ""),
    });
    (attributes, count)
}

/// A sum that only grows: the count of something that happened.
#[derive(Debug)]
pub struct Counter<const LABELS: usize> {
    name: &'static str,
    unit: &'static str,
    label_keys: &'static [&'static str; LABELS],
    instrument: OnceLock<opentelemetry::metrics::Counter<f64>>,
}

impl<const LABELS: usize> Counter<LABELS> {
    /// Declares a counter named `name`, in `unit`, whose values name the `label_keys`.
    #[must_use]
    pub const fn declare(
        name: &'static str,
        unit: &'static str,
        label_keys: &'static [&'static str; LABELS],
    ) -> Self {
        Self {
            name,
            unit,
            label_keys,
            instrument: OnceLock::new(),
        }
    }

    /// Selects the series the label `values` name, in declaration order.
    pub const fn labeled(&self, values: [&'static str; LABELS]) -> CounterSelection<'_, LABELS> {
        CounterSelection {
            counter: self,
            labels: values,
        }
    }

    /// The instrument, built from the installed meter on first use.
    fn instrument(&self) -> Option<&opentelemetry::metrics::Counter<f64>> {
        built(&self.instrument, |meter| {
            meter.f64_counter(self.name).with_unit(self.unit).build()
        })
    }

    fn add_labeled(&self, labels: [&'static str; LABELS], value: f64) {
        if let Some(counter) = self.instrument() {
            let (attributes, count) = attributes(self.label_keys, labels);
            counter.add(value, &attributes[..count]);
        }
    }

    /// Builds the instrument now when a meter is installed, so [`Self::add_built`] finds
    /// it.
    pub(crate) fn build(&self) {
        let _ = self.instrument();
    }

    /// Adds `value` to the series the label `values` select, through an instrument
    /// [`Self::build`] already built, and records nothing before then.
    ///
    /// It never builds the instrument: the SDK reports each instrument it builds as a
    /// `tracing` event, so a recording from inside a `tracing` layer that built would
    /// re-enter that layer while the instrument's cell is still being filled.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a count past 2^53 events loses its last digits, which no reader acts on"
    )]
    pub(crate) fn add_built(&self, labels: [&'static str; LABELS], value: u64) {
        if let Some(counter) = self.instrument.get() {
            let (attributes, count) = attributes(self.label_keys, labels);
            counter.add(value as f64, &attributes[..count]);
        }
    }
}

impl Counter<0> {
    /// Adds `value` to the counter.
    pub fn add(&self, value: u64) {
        self.labeled([]).add(value);
    }
}

/// One series of a counter, selected by its label values; [`Self::add`] records.
#[derive(Clone, Copy, Debug)]
#[must_use = "a selection records nothing until `add` runs"]
pub struct CounterSelection<'counter, const LABELS: usize> {
    counter: &'counter Counter<LABELS>,
    labels: [&'static str; LABELS],
}

impl<const LABELS: usize> CounterSelection<'_, LABELS> {
    /// Adds `value` to the selected series.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a count past 2^53 events loses its last digits, which no reader acts on"
    )]
    pub fn add(self, value: u64) {
        self.counter.add_labeled(self.labels, value as f64);
    }
}

/// The value types a gauge records: a byte or item count, or a fractional quantity.
pub trait GaugeValue: Copy + private::Sealed {
    /// The value as the instrument records it, or `None` for a value that measures
    /// nothing: a negative, infinite, or NaN float.
    fn measured(self) -> Option<f64>;
}

impl GaugeValue for u64 {
    #[expect(
        clippy::cast_precision_loss,
        reason = "a gauge past 2^53 bytes or items loses its last digits, which no reader acts on"
    )]
    fn measured(self) -> Option<f64> {
        Some(self as f64)
    }
}

impl GaugeValue for f64 {
    fn measured(self) -> Option<f64> {
        (self.is_finite() && self >= 0.0).then_some(self)
    }
}

/// The value types a histogram records: a duration, recorded in seconds, or a count.
pub trait HistogramValue: Copy + private::Sealed {
    /// What the OpenTelemetry instrument records.
    #[doc(hidden)]
    type Recorded: std::fmt::Debug + Send + Sync + 'static;

    /// The OpenTelemetry histogram named `name`, in `unit`, bucketed at `boundaries`.
    #[doc(hidden)]
    fn histogram(
        meter: &Meter,
        name: &'static str,
        unit: &'static str,
        boundaries: &'static [f64],
    ) -> opentelemetry::metrics::Histogram<Self::Recorded>;

    /// The value as the instrument records it.
    #[doc(hidden)]
    fn recorded(self) -> Self::Recorded;
}

impl HistogramValue for Duration {
    type Recorded = f64;

    fn histogram(
        meter: &Meter,
        name: &'static str,
        unit: &'static str,
        boundaries: &'static [f64],
    ) -> opentelemetry::metrics::Histogram<f64> {
        meter
            .f64_histogram(name)
            .with_unit(unit)
            .with_boundaries(boundaries.to_vec())
            .build()
    }

    fn recorded(self) -> f64 {
        self.as_secs_f64()
    }
}

impl HistogramValue for u64 {
    type Recorded = Self;

    fn histogram(
        meter: &Meter,
        name: &'static str,
        unit: &'static str,
        boundaries: &'static [f64],
    ) -> opentelemetry::metrics::Histogram<Self> {
        meter
            .u64_histogram(name)
            .with_unit(unit)
            .with_boundaries(boundaries.to_vec())
            .build()
    }

    fn recorded(self) -> Self {
        self
    }
}

mod private {
    use std::time::Duration;

    /// Seals [`GaugeValue`](super::GaugeValue) and
    /// [`HistogramValue`](super::HistogramValue) to the value types they record.
    pub trait Sealed {}
    impl Sealed for u64 {}
    impl Sealed for f64 {}
    impl Sealed for Duration {}
}

/// The latest value of a quantity, such as resident memory in bytes.
#[derive(Debug)]
pub struct Gauge<Value: GaugeValue, const LABELS: usize> {
    name: &'static str,
    unit: &'static str,
    label_keys: &'static [&'static str; LABELS],
    instrument: OnceLock<opentelemetry::metrics::Gauge<f64>>,
    _value: PhantomData<fn(Value)>,
}

impl<Value: GaugeValue, const LABELS: usize> Gauge<Value, LABELS> {
    /// Declares a gauge named `name`, in `unit`, whose values name the `label_keys`.
    #[must_use]
    pub const fn declare(
        name: &'static str,
        unit: &'static str,
        label_keys: &'static [&'static str; LABELS],
    ) -> Self {
        Self {
            name,
            unit,
            label_keys,
            instrument: OnceLock::new(),
            _value: PhantomData,
        }
    }

    /// Selects `value` for the series the label `values` name.
    pub const fn labeled_value(
        &self,
        labels: [&'static str; LABELS],
        value: Value,
    ) -> GaugeSelection<'_, Value, LABELS> {
        GaugeSelection {
            gauge: self,
            labels,
            value,
        }
    }
}

impl<Value: GaugeValue> Gauge<Value, 0> {
    /// Selects `value`, a caller-measured quantity; [`GaugeSelection::record`] records it.
    pub const fn value(&self, value: Value) -> GaugeSelection<'_, Value, 0> {
        self.labeled_value([], value)
    }
}

/// One value of a gauge, selected but not yet recorded; [`Self::record`] records it.
#[derive(Clone, Copy, Debug)]
#[must_use = "a selection records nothing until `record` runs"]
pub struct GaugeSelection<'gauge, Value: GaugeValue, const LABELS: usize> {
    gauge: &'gauge Gauge<Value, LABELS>,
    labels: [&'static str; LABELS],
    value: Value,
}

impl<Value: GaugeValue, const LABELS: usize> GaugeSelection<'_, Value, LABELS> {
    /// Records the selected value.
    ///
    /// A value that measures nothing records nothing: a negative, infinite, or NaN float.
    /// It never records zero in its place.
    pub fn record(self) {
        let gauge = self.gauge;
        let Some(measured) = self.value.measured() else {
            return;
        };
        let built = built(&gauge.instrument, |meter| {
            meter.f64_gauge(gauge.name).with_unit(gauge.unit).build()
        });
        if let Some(instrument) = built {
            let (attributes, count) = attributes(gauge.label_keys, self.labels);
            instrument.record(measured, &attributes[..count]);
        }
    }
}

/// Counts of recorded values per bucket, with their count and sum: durations in seconds,
/// or counts, such as the statements one transaction ran.
#[derive(Debug)]
pub struct Histogram<const LABELS: usize, Value: HistogramValue = Duration> {
    name: &'static str,
    unit: &'static str,
    label_keys: &'static [&'static str; LABELS],
    boundaries: &'static [f64],
    instrument: OnceLock<opentelemetry::metrics::Histogram<Value::Recorded>>,
}

impl<const LABELS: usize> Histogram<LABELS> {
    /// Declares a duration histogram named `name`, in seconds, whose values name the
    /// `label_keys`, bucketed at [`DURATION_BOUNDARIES_SECONDS`].
    #[must_use]
    pub const fn declare(name: &'static str, label_keys: &'static [&'static str; LABELS]) -> Self {
        Self {
            name,
            unit: "s",
            label_keys,
            boundaries: &DURATION_BOUNDARIES_SECONDS,
            instrument: OnceLock::new(),
        }
    }
}

impl<const LABELS: usize> Histogram<LABELS, u64> {
    /// Declares a histogram of counts named `name`, in `unit`, whose values name the
    /// `label_keys`, bucketed at the ascending upper bounds `boundaries`.
    #[must_use]
    pub const fn declare_count(
        name: &'static str,
        unit: &'static str,
        label_keys: &'static [&'static str; LABELS],
        boundaries: &'static [f64],
    ) -> Self {
        Self {
            name,
            unit,
            label_keys,
            boundaries,
            instrument: OnceLock::new(),
        }
    }
}

impl<const LABELS: usize, Value: HistogramValue> Histogram<LABELS, Value> {
    /// Buckets at `boundaries`, ascending upper bounds in the instrument's unit.
    #[must_use]
    pub const fn boundaries(mut self, boundaries: &'static [f64]) -> Self {
        self.boundaries = boundaries;
        self
    }

    /// Selects the series the label `values` name, in declaration order.
    pub const fn labeled(
        &self,
        values: [&'static str; LABELS],
    ) -> HistogramSelection<'_, LABELS, Value> {
        HistogramSelection {
            histogram: self,
            labels: values,
        }
    }
}

impl<Value: HistogramValue> Histogram<0, Value> {
    /// Records one value.
    pub fn record(&self, value: Value) {
        self.labeled([]).record(value);
    }
}

/// One series of a histogram, selected by its label values; [`Self::record`] records.
#[derive(Clone, Copy, Debug)]
#[must_use = "a selection records nothing until `record` runs"]
pub struct HistogramSelection<'histogram, const LABELS: usize, Value: HistogramValue = Duration> {
    histogram: &'histogram Histogram<LABELS, Value>,
    labels: [&'static str; LABELS],
}

impl<const LABELS: usize, Value: HistogramValue> HistogramSelection<'_, LABELS, Value> {
    /// Records one value into the selected series.
    pub fn record(self, value: Value) {
        let histogram = self.histogram;
        let built = built(&histogram.instrument, |meter| {
            Value::histogram(meter, histogram.name, histogram.unit, histogram.boundaries)
        });
        if let Some(instrument) = built {
            let (attributes, count) = attributes(histogram.label_keys, self.labels);
            instrument.record(value.recorded(), &attributes[..count]);
        }
    }
}

/// Reads one [`ObservableUpDownCounter`] holds registered, at most: three databases per
/// workspace (index, vectors, metrics) times the 64 workspaces one repository process may
/// retain, `SERVER_WORKSPACES_MAX`. One declaration serves at most two of the three, so the
/// bound leaves room for a workspace that reopens before its earlier owner dropped.
pub const OBSERVATIONS_MAX: usize = 192;

/// One registered read of an instrument with `LABELS` label keys.
type Read<const LABELS: usize> = dyn Fn(&Observation<'_, LABELS>) + Send + Sync;

/// A quantity its owner holds, read when the meter collects: a file's size in bytes, or
/// the commands a queue holds.
///
/// The exported instrument is an OpenTelemetry `ObservableUpDownCounter`, cumulative, so a
/// collector reads the latest value of each series. A series no read observed in a
/// collection leaves the export, so an owner that dropped its guard stops reporting.
///
/// The instrument registers one SDK callback, when its first read registers with a meter
/// installed. A collection takes the list's lock to copy out its reads, at most
/// [`OBSERVATIONS_MAX`] `Arc` clones, and runs them with the lock released.
pub struct ObservableUpDownCounter<const LABELS: usize> {
    name: &'static str,
    unit: &'static str,
    label_keys: &'static [&'static str; LABELS],
    /// The live reads, by the identity their guard removes them with.
    reads: Mutex<Vec<(u64, Arc<Read<LABELS>>)>>,
    /// The identity the next read gets.
    next: AtomicU64,
    /// Set once the SDK callback is registered.
    callback: OnceLock<()>,
    /// Whether a read past [`OBSERVATIONS_MAX`] was refused and recorded.
    refusal_recorded: AtomicBool,
}

impl<const LABELS: usize> std::fmt::Debug for ObservableUpDownCounter<LABELS> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObservableUpDownCounter")
            .field("name", &self.name)
            .field("unit", &self.unit)
            .field("reads", &self.lock().len())
            .finish_non_exhaustive()
    }
}

impl<const LABELS: usize> ObservableUpDownCounter<LABELS> {
    /// Declares a quantity named `name`, in `unit`, whose values name the `label_keys`.
    #[must_use]
    pub const fn declare(
        name: &'static str,
        unit: &'static str,
        label_keys: &'static [&'static str; LABELS],
    ) -> Self {
        Self {
            name,
            unit,
            label_keys,
            reads: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
            callback: OnceLock::new(),
            refusal_recorded: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(u64, Arc<Read<LABELS>>)>> {
        self.reads.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers `read`, which runs on the SDK reader's task each time it collects, until
    /// the returned guard drops; the guard's drop removes it from the list.
    ///
    /// Answers `None`, and registers nothing, when the process installed no meter, or when
    /// [`OBSERVATIONS_MAX`] reads are registered: the first such refusal is recorded once
    /// as a `WARN` record naming the instrument. The reads of one collection run in series
    /// with no timeout, so `read` takes no lock a request holds, never waits, and holds its
    /// owner weakly.
    ///
    /// ```
    /// static QUEUED: rift_tracing::ObservableUpDownCounter<1> =
    ///     rift_tracing::ObservableUpDownCounter::declare("test.queue.length", "{task}", &["queue"]);
    /// // `None` in a process that installed no meter.
    /// let guard = QUEUED.observe(|observation| observation.observe(["parse"], 3));
    /// drop(guard);
    /// ```
    pub fn observe(
        &'static self,
        read: impl Fn(&Observation<'_, LABELS>) + Send + Sync + 'static,
    ) -> Option<ObservationGuard> {
        if !meter_installed() {
            return None;
        }
        let identity = {
            let mut reads = self.lock();
            if reads.len() >= OBSERVATIONS_MAX {
                drop(reads);
                if !self.refusal_recorded.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        target: "rift_tracing::metrics",
                        instrument = self.name,
                        observations_max = OBSERVATIONS_MAX,
                        "observable instrument refused a read past its bound"
                    );
                }
                return None;
            }
            let identity = self.next.fetch_add(1, Ordering::Relaxed);
            reads.push((identity, Arc::new(read)));
            identity
        };
        self.callback.get_or_init(|| {
            with_meter(|meter| {
                let _instrument = meter
                    .i64_observable_up_down_counter(self.name)
                    .with_unit(self.unit)
                    .with_callback(move |instrument| self.collect(instrument))
                    .build();
            });
        });
        Some(ObservationGuard {
            instrument: self,
            identity,
        })
    }

    /// Runs every registered read against `instrument`, the list's lock released first.
    fn collect(&self, instrument: &dyn AsyncInstrument<i64>) {
        let reads: Vec<Arc<Read<LABELS>>> = self
            .lock()
            .iter()
            .map(|(_, read)| Arc::clone(read))
            .collect();
        let observation = Observation {
            instrument,
            label_keys: self.label_keys,
        };
        for read in reads {
            read(&observation);
        }
    }

    /// The reads registered now.
    #[cfg(test)]
    pub(crate) fn registered(&self) -> usize {
        self.lock().len()
    }

    /// Whether the SDK callback is registered.
    #[cfg(test)]
    pub(crate) fn callback_registered(&self) -> bool {
        self.callback.get().is_some()
    }
}

/// An instrument a guard removes its read from.
trait Unregister: Sync {
    fn unregister(&self, identity: u64);
}

impl<const LABELS: usize> Unregister for ObservableUpDownCounter<LABELS> {
    fn unregister(&self, identity: u64) {
        self.lock().retain(|(held, _)| *held != identity);
    }
}

/// One collection's view of an [`ObservableUpDownCounter`]; [`Self::observe`] reports a
/// value.
pub struct Observation<'collection, const LABELS: usize> {
    instrument: &'collection dyn AsyncInstrument<i64>,
    label_keys: &'static [&'static str; LABELS],
}

impl<const LABELS: usize> Observation<'_, LABELS> {
    /// Reports `value` for the series the label `values` name, in declaration order. A
    /// value past `i64::MAX` reports `i64::MAX`.
    pub fn observe(&self, labels: [&'static str; LABELS], value: u64) {
        let (attributes, count) = attributes(self.label_keys, labels);
        self.instrument.observe(
            i64::try_from(value).unwrap_or(i64::MAX),
            &attributes[..count],
        );
    }
}

impl<const LABELS: usize> std::fmt::Debug for Observation<'_, LABELS> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Observation")
            .field("label_keys", &self.label_keys)
            .finish_non_exhaustive()
    }
}

/// Keeps a read [`ObservableUpDownCounter::observe`] registered: dropping it removes the
/// read from the instrument's list, so the next collection runs it no more.
#[must_use = "dropping the guard stops the read"]
pub struct ObservationGuard {
    instrument: &'static dyn Unregister,
    identity: u64,
}

impl std::fmt::Debug for ObservationGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObservationGuard")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl Drop for ObservationGuard {
    fn drop(&mut self) {
        self.instrument.unregister(self.identity);
    }
}

/// The duration of every `traced!` operation, the name the OpenTelemetry Collector's span
/// metrics connector derives from spans.
pub(crate) static OPERATION_DURATION: Histogram<4> = Histogram::declare(
    "traces.span.metrics.duration",
    &["span.name", "span.kind", "status.code", "error.type"],
);
/// The count of every `traced!` operation, beside [`OPERATION_DURATION`].
pub(crate) static OPERATION_CALLS: Counter<4> = Counter::declare(
    "traces.span.metrics.calls",
    "{call}",
    &["span.name", "span.kind", "status.code", "error.type"],
);

/// The `span.kind` of every `traced!` operation: no operation states `otel.kind`, so the
/// span it exports takes the SDK's default kind, `SpanKind::Internal`, and the metrics name
/// the kind the exported span carries.
const SPAN_KIND_INTERNAL: &str = "Internal";

/// The `status.code` of an operation that finished.
const STATUS_OK: &str = "Ok";
/// The `status.code` of an operation that failed, panicked, or was cancelled.
const STATUS_ERROR: &str = "Error";
/// The `error.type` of an operation that panicked.
const ERROR_PANIC: &str = "panic";
/// The `error.type` of an awaited operation dropped before it finished.
const ERROR_CANCELLED: &str = "cancelled";

/// How an operation that ends without a panic ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// The work finished: a block left by any path, or a future that returned.
    Finished,
    /// The awaited work was dropped before it returned.
    Cancelled,
    /// The awaited work returned after its span recorded the failure with this
    /// `error.type` label.
    Failed(&'static str),
}

/// The completion of one `traced!` operation: its duration and its outcome, recorded once
/// when the guard drops.
///
/// The guard reads the monotonic clock only when a meter is installed, so a process that
/// records no metrics pays one atomic read per operation. Retained clones of the
/// operation's span do not lengthen the recorded duration: the guard drops when the work
/// ends.
#[doc(hidden)]
#[derive(Debug)]
#[must_use = "the completion records when it drops"]
pub struct Completion {
    operation: &'static str,
    started: Option<Duration>,
    ending: Ending,
    span: Option<tracing::span::Id>,
}

impl Completion {
    /// The guard of an operation whose open span `span` the guard reads, as it drops, for
    /// a failure the span recorded: an `error.type`, or an `outcome` that is not a
    /// completion. The guard drops before the span closes.
    #[doc(hidden)]
    pub fn of_span(mut self, span: Option<tracing::span::Id>) -> Self {
        self.span = span;
        self
    }

    /// Marks an awaited operation's work as returned, under `span`, still open: failed
    /// when the span recorded a failure, finished otherwise.
    pub(crate) fn finished(&mut self, span: &tracing::Span) {
        self.ending = match self.started.and(span.id()).as_ref().and_then(span_failure) {
            Some(label) => Ending::Failed(label),
            None => Ending::Finished,
        };
    }

    /// Whether the guard reads the clock and records when it drops.
    #[cfg(test)]
    pub(crate) const fn records(&self) -> bool {
        self.started.is_some()
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        let Some(started) = self.started else {
            return;
        };
        // A clock that moved backwards gives no duration; the call still counts.
        let elapsed = monotonic_now().checked_sub(started);
        let (status, error) = if std::thread::panicking() {
            (STATUS_ERROR, ERROR_PANIC)
        } else {
            match self.ending {
                Ending::Cancelled => (STATUS_ERROR, ERROR_CANCELLED),
                Ending::Failed(label) => (STATUS_ERROR, label),
                Ending::Finished => match self.span.as_ref().and_then(span_failure) {
                    Some(label) => (STATUS_ERROR, label),
                    None => (STATUS_OK, ""),
                },
            }
        };
        let labels = [self.operation, SPAN_KIND_INTERNAL, status, error];
        OPERATION_CALLS.labeled(labels).add(1);
        if let Some(elapsed) = elapsed {
            OPERATION_DURATION.labeled(labels).record(elapsed);
        }
    }
}

/// Starts the completion guard of a `traced!` block: it ends when the block is left.
#[doc(hidden)]
pub fn completion(operation: &'static str) -> Completion {
    started(operation, Ending::Finished)
}

/// Starts the completion guard of an awaited `traced!` operation at its first poll: it
/// counts as cancelled unless [`Completion::finished`] runs before it drops.
pub(crate) fn future_completion(operation: &'static str) -> Completion {
    started(operation, Ending::Cancelled)
}

/// A completion guard of `operation` that ends as `ending` unless told otherwise.
fn started(operation: &'static str, ending: Ending) -> Completion {
    Completion {
        operation,
        started: meter_installed().then(monotonic_now),
        ending,
        span: None,
    }
}

#[cfg(test)]
mod tests;
