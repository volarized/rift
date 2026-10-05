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
//! module holds no value. The process installs at most one meter: the `otlp` export when an
//! endpoint is configured, or a test's `ScopedRecorder`. Before that, and in a process that
//! installs none, a recording reads two atomics and records nothing.
//!
//! A meter obtained from `opentelemetry::global` before a provider is set stays a no-op for
//! good, so the meter is never taken from there: the installer hands it over once, and
//! each declaration builds its instrument from it lazily.

use std::marker::PhantomData;
use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Meter;

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
#[cfg(any(test, feature = "fixtures", feature = "otlp"))]
pub(crate) fn scope() -> opentelemetry::InstrumentationScope {
    opentelemetry::InstrumentationScope::builder(env!("CARGO_PKG_NAME"))
        .with_version(env!("CARGO_PKG_VERSION"))
        .build()
}

/// Makes `meter` the one every declaration builds its instrument from; a meter installed
/// before stays, and `meter` is dropped.
#[cfg(any(test, feature = "fixtures", feature = "otlp"))]
pub(crate) fn install_meter(meter: Meter) {
    let _ = METER.set(meter);
}

/// Whether a meter is installed, so a recording reaches an instrument.
pub(crate) fn meter_installed() -> bool {
    METER.get().is_some()
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

    fn add_labeled(&self, labels: [&'static str; LABELS], value: f64) {
        let built = built(&self.instrument, |meter| {
            meter.f64_counter(self.name).with_unit(self.unit).build()
        });
        if let Some(counter) = built {
            let (attributes, count) = attributes(self.label_keys, labels);
            counter.add(value, &attributes[..count]);
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

    /// Adds `value`, a fractional quantity such as CPU seconds, to the selected series.
    pub(crate) fn add_fraction(self, value: f64) {
        self.counter.add_labeled(self.labels, value);
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
    /// What one recorded unit is in the instrument's unit: `0.01` for a percent recorded
    /// into a utilization whose unit is `1`.
    scale: f64,
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
            scale: 1.0,
            instrument: OnceLock::new(),
            _value: PhantomData,
        }
    }

    /// Records each value multiplied by `scale`, the instrument's units per recorded unit.
    #[must_use]
    pub const fn scaled(mut self, scale: f64) -> Self {
        self.scale = scale;
        self
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
            instrument.record(measured * gauge.scale, &attributes[..count]);
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

/// The instruments `rift-tracing` declares and Rift code records into.
#[derive(Debug)]
#[non_exhaustive]
pub struct Metrics {
    /// The process's CPU usage, in percent of one core; past 100 on several cores. The
    /// exported instrument is `process.cpu.utilization`, unit `1`.
    pub cpu: Gauge<f64, 0>,
    /// The process's resident set, in bytes: `process.memory.usage`.
    pub memory: Gauge<u64, 0>,
}

/// The instruments every Rift crate records into.
///
/// ```
/// let metrics = rift_tracing::metrics();
/// metrics.memory.value(4 << 20).record();
/// metrics.cpu.value(12.5).record();
/// ```
#[must_use]
pub fn metrics() -> &'static Metrics {
    &METRICS
}

static METRICS: Metrics = Metrics {
    cpu: Gauge::declare("process.cpu.utilization", "1", &[]).scaled(0.01),
    memory: Gauge::declare("process.memory.usage", "By", &[]),
};

/// The duration of every `traced!` operation, the name the OpenTelemetry Collector's span
/// metrics connector derives from spans.
pub(crate) static OPERATION_DURATION: Histogram<3> = Histogram::declare(
    "traces.span.metrics.duration",
    &["span.name", "status.code", "error.type"],
);
/// The count of every `traced!` operation, beside [`OPERATION_DURATION`].
pub(crate) static OPERATION_CALLS: Counter<3> = Counter::declare(
    "traces.span.metrics.calls",
    "{call}",
    &["span.name", "status.code", "error.type"],
);

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
        let labels = [self.operation, status, error];
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
