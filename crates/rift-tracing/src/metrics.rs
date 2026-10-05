//! Typed metric instruments: counters, gauges, and histograms a declaration fixes once.
//!
//! A declaration names an instrument, its unit, its kind, the label keys it accepts, and
//! its bounds. A caller holds the declared handle and records values into it; it never
//! registers an instrument or builds a label map on the recording path. Label values are
//! `&'static str`, so a value comes from a closed set the code spells out, and a path, a
//! query, or an error message cannot become a label.
//!
//! A value lands in the metric values of the thread's current `tracing` dispatcher: the
//! ones [`TracingRuntime`](crate::TracingRuntime) installs, or a test's `ScopedRecorder`.
//! A thread whose dispatcher holds neither records nothing, and pays one dispatcher lookup.

mod values;

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use tracing::Subscriber;
use tracing_subscriber::Layer;

use crate::measurement::monotonic_now;
use crate::sampler::ProcessSample;

pub(crate) use values::{Labels, MetricValues};
pub use values::{MetricSeries, MetricSnapshot, SeriesValue};

/// Label keys one instrument may declare, at most.
pub const METRIC_LABELS_MAX: usize = 4;
/// Label sets one instrument keeps when its declaration names no other bound. A label set
/// past it records into the instrument's overflow series.
pub const METRIC_SERIES_MAX_DEFAULT: usize = 256;
/// Bucket boundaries one histogram may declare, at most.
pub const HISTOGRAM_BOUNDARIES_MAX: usize = 16;
/// Upper bucket boundaries, in seconds, of a duration histogram whose declaration names no
/// others: the boundaries the OpenTelemetry HTTP semantic conventions advise for durations.
pub const DURATION_BOUNDARIES_SECONDS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.075, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

/// What an instrument holds between two reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstrumentKind {
    /// A sum that only grows, such as dropped records.
    Counter,
    /// The latest value recorded, such as resident memory.
    Gauge,
    /// Counts of recorded values per bucket, with their count and sum.
    Histogram,
}

/// One instrument as its declaration fixes it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Instrument {
    name: &'static str,
    unit: &'static str,
    kind: InstrumentKind,
    label_keys: &'static [&'static str],
    series_max: usize,
    boundaries: &'static [f64],
}

impl Instrument {
    /// Declares an instrument, refusing at compile time, in a `const` declaration, more
    /// than [`METRIC_LABELS_MAX`] label keys.
    const fn declared(
        name: &'static str,
        unit: &'static str,
        kind: InstrumentKind,
        label_keys: &'static [&'static str],
    ) -> Self {
        assert!(
            label_keys.len() <= METRIC_LABELS_MAX,
            "an instrument declares at most METRIC_LABELS_MAX label keys"
        );
        Self {
            name,
            unit,
            kind,
            label_keys,
            series_max: METRIC_SERIES_MAX_DEFAULT,
            boundaries: &[],
        }
    }

    /// The instrument's name, such as `process.memory.usage`.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The unit its values carry, in the OpenTelemetry spelling: `s`, `By`, `1`, or a
    /// curly-brace annotation such as `{call}`.
    #[must_use]
    pub const fn unit(&self) -> &'static str {
        self.unit
    }

    /// What the instrument holds between two reads.
    #[must_use]
    pub const fn kind(&self) -> InstrumentKind {
        self.kind
    }

    /// The label keys every recorded value names a value for, in declaration order.
    #[must_use]
    pub const fn label_keys(&self) -> &'static [&'static str] {
        self.label_keys
    }

    /// Label sets the instrument keeps before it records into its overflow series.
    #[must_use]
    pub const fn series_max(&self) -> usize {
        self.series_max
    }

    /// The upper bucket boundaries of a histogram, ascending; empty for other kinds.
    #[must_use]
    pub const fn boundaries(&self) -> &'static [f64] {
        self.boundaries
    }
}

/// Runs `record` against the metric values of the thread's current dispatcher, when it
/// holds any.
fn with_installed(mut record: impl FnMut(&MetricValues)) {
    tracing::dispatcher::get_default(|dispatch| {
        if let Some(layer) = dispatch.downcast_ref::<MetricLayer>() {
            record(&layer.values);
        }
    });
}

/// Whether the thread's current dispatcher holds metric values.
fn installed() -> bool {
    tracing::dispatcher::get_default(|dispatch| dispatch.downcast_ref::<MetricLayer>().is_some())
}

/// The label values a recording names, in declaration order, padded to [`Labels`].
fn labels<const LABELS: usize>(values: [&'static str; LABELS]) -> Labels {
    const {
        assert!(
            LABELS <= METRIC_LABELS_MAX,
            "at most METRIC_LABELS_MAX label values"
        );
    };
    let mut labels = [""; METRIC_LABELS_MAX];
    labels[..LABELS].copy_from_slice(&values);
    labels
}

/// A sum that only grows: the count of something that happened.
#[derive(Clone, Copy, Debug)]
pub struct Counter<const LABELS: usize> {
    instrument: Instrument,
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
            instrument: Instrument::declared(name, unit, InstrumentKind::Counter, label_keys),
        }
    }

    /// Keeps at most `series_max` label sets; later ones record into the overflow series.
    #[must_use]
    pub const fn series_max(mut self, series_max: usize) -> Self {
        self.instrument.series_max = series_max;
        self
    }

    /// The declaration this handle records into.
    #[must_use]
    pub const fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    /// Selects the series the label `values` name, in declaration order.
    pub fn labeled(&self, values: [&'static str; LABELS]) -> CounterSelection<'_> {
        CounterSelection {
            instrument: &self.instrument,
            labels: labels(values),
        }
    }

    /// Adds `value`, in the instrument's unit, straight into `values`, for the sampler that
    /// owns them. A fractional value, such as CPU seconds, keeps its fraction.
    pub(crate) fn add_into(
        &self,
        values: &MetricValues,
        labels: [&'static str; LABELS],
        value: f64,
    ) {
        values.add(&self.instrument, self::labels(labels), value);
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
pub struct CounterSelection<'counter> {
    instrument: &'counter Instrument,
    labels: Labels,
}

impl CounterSelection<'_> {
    /// Adds `value` to the selected series.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a count past 2^53 events loses its last digits, which no reader acts on"
    )]
    pub fn add(self, value: u64) {
        with_installed(|values| values.add(self.instrument, self.labels, value as f64));
    }
}

/// The value types a gauge records: a byte or item count, or a fractional quantity.
pub trait GaugeValue: Copy + private::Sealed {
    /// The value as the instrument stores it, or `None` for a value that measures nothing:
    /// a negative, infinite, or NaN float.
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

mod private {
    /// Seals [`GaugeValue`](super::GaugeValue) to the value types its storage keeps whole.
    pub trait Sealed {}
    impl Sealed for u64 {}
    impl Sealed for f64 {}
}

/// The latest value of a quantity, such as resident memory in bytes.
#[derive(Clone, Copy, Debug)]
pub struct Gauge<Value: GaugeValue, const LABELS: usize> {
    instrument: Instrument,
    /// What one recorded unit is in the instrument's unit: `0.01` for a percent recorded
    /// into a utilization whose unit is `1`.
    scale: f64,
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
            instrument: Instrument::declared(name, unit, InstrumentKind::Gauge, label_keys),
            scale: 1.0,
            _value: PhantomData,
        }
    }

    /// Records each value multiplied by `scale`, the instrument's units per recorded unit.
    #[must_use]
    pub const fn scaled(mut self, scale: f64) -> Self {
        self.scale = scale;
        self
    }

    /// Keeps at most `series_max` label sets; later ones record into the overflow series.
    #[must_use]
    pub const fn series_max(mut self, series_max: usize) -> Self {
        self.instrument.series_max = series_max;
        self
    }

    /// The declaration this handle records into.
    #[must_use]
    pub const fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    /// Selects `value` for the series the label `values` name.
    pub fn labeled_value(
        &self,
        labels: [&'static str; LABELS],
        value: Value,
    ) -> GaugeSelection<'_, Value> {
        GaugeSelection {
            gauge: self.erased(),
            labels: self::labels(labels),
            value: Selected::Value(value),
        }
    }

    /// Records `value` straight into `values`, for the sampler that owns them.
    pub(crate) fn record_into(
        &self,
        values: &MetricValues,
        labels: [&'static str; LABELS],
        value: Value,
    ) {
        if let Some(measured) = value.measured() {
            values.set(
                &self.instrument,
                self::labels(labels),
                measured * self.scale,
            );
        }
    }

    const fn erased(&self) -> ErasedGauge<'_> {
        ErasedGauge {
            instrument: &self.instrument,
            scale: self.scale,
        }
    }
}

impl<Value: GaugeValue> Gauge<Value, 0> {
    /// Selects `value`, a caller-measured quantity; [`GaugeSelection::record`] records it.
    pub fn value(&self, value: Value) -> GaugeSelection<'_, Value> {
        self.labeled_value([], value)
    }
}

/// A gauge without its value type, as a selection carries it.
#[derive(Clone, Copy, Debug)]
struct ErasedGauge<'gauge> {
    instrument: &'gauge Instrument,
    scale: f64,
}

/// Where a gauge selection takes its value from.
#[derive(Clone, Copy, Debug)]
enum Selected<Value> {
    /// A value the caller supplied.
    Value(Value),
    /// The latest published process sample, read when `record` runs.
    Current(fn(&ProcessSample) -> Option<Value>),
}

/// One value of a gauge, selected but not yet recorded; [`Self::record`] records it.
#[derive(Clone, Copy, Debug)]
#[must_use = "a selection records nothing until `record` runs"]
pub struct GaugeSelection<'gauge, Value: GaugeValue> {
    gauge: ErasedGauge<'gauge>,
    labels: Labels,
    value: Selected<Value>,
}

impl<Value: GaugeValue> GaugeSelection<'_, Value> {
    /// Records the selected value.
    ///
    /// A value that measures nothing records nothing: a negative, infinite, or NaN float,
    /// or a current value the process sampler has not published. It never records zero
    /// in their place.
    pub fn record(self) {
        let gauge = self.gauge;
        let labels = self.labels;
        let selected = self.value;
        with_installed(|values| {
            let value = match selected {
                Selected::Value(value) => Some(value),
                Selected::Current(read) => values.latest_sample().as_ref().and_then(read),
            };
            if let Some(measured) = value.and_then(GaugeValue::measured) {
                values.set(gauge.instrument, labels, measured * gauge.scale);
            }
        });
    }
}

/// A gauge of the current process, whose current value the process sampler publishes.
#[derive(Clone, Copy, Debug)]
pub struct ProcessGauge<Value: GaugeValue> {
    gauge: Gauge<Value, 0>,
    current: fn(&ProcessSample) -> Option<Value>,
}

impl<Value: GaugeValue> ProcessGauge<Value> {
    /// The handle over `gauge` whose current value `current` reads from a sample.
    pub(crate) const fn reading(
        gauge: Gauge<Value, 0>,
        current: fn(&ProcessSample) -> Option<Value>,
    ) -> Self {
        Self { gauge, current }
    }

    /// Selects the value of the latest published process sample, read when `record` runs.
    ///
    /// The selection performs no OS read: the sampler refreshes the process on its own
    /// tick, and a value it has not published records nothing.
    pub fn current(&self) -> GaugeSelection<'_, Value> {
        GaugeSelection {
            gauge: self.gauge.erased(),
            labels: labels([]),
            value: Selected::Current(self.current),
        }
    }

    /// Selects `value`, a caller-measured quantity in the same unit as [`Self::current`].
    pub fn value(&self, value: Value) -> GaugeSelection<'_, Value> {
        self.gauge.value(value)
    }

    /// The declaration this handle records into.
    #[must_use]
    pub const fn instrument(&self) -> &Instrument {
        self.gauge.instrument()
    }

    /// Records the current value of `sample` straight into `values`, for the sampler.
    pub(crate) fn record_sample_into(&self, values: &MetricValues, sample: &ProcessSample) {
        if let Some(value) = (self.current)(sample) {
            self.gauge.record_into(values, [], value);
        }
    }
}

/// Counts of recorded durations per bucket, with their count and sum in seconds.
#[derive(Clone, Copy, Debug)]
pub struct Histogram<const LABELS: usize> {
    instrument: Instrument,
}

impl<const LABELS: usize> Histogram<LABELS> {
    /// Declares a duration histogram named `name`, in seconds, whose values name the
    /// `label_keys`, bucketed at [`DURATION_BOUNDARIES_SECONDS`].
    #[must_use]
    pub const fn declare(name: &'static str, label_keys: &'static [&'static str; LABELS]) -> Self {
        let mut instrument = Instrument::declared(name, "s", InstrumentKind::Histogram, label_keys);
        instrument.boundaries = &DURATION_BOUNDARIES_SECONDS;
        Self { instrument }
    }

    /// Buckets at `boundaries`, ascending upper bounds in seconds.
    ///
    /// # Panics
    ///
    /// Panics on more than [`HISTOGRAM_BOUNDARIES_MAX`] boundaries; in a `const`
    /// declaration, that fails compilation.
    #[must_use]
    pub const fn boundaries(mut self, boundaries: &'static [f64]) -> Self {
        assert!(
            boundaries.len() <= HISTOGRAM_BOUNDARIES_MAX,
            "a histogram declares at most HISTOGRAM_BOUNDARIES_MAX boundaries"
        );
        self.instrument.boundaries = boundaries;
        self
    }

    /// Keeps at most `series_max` label sets; later ones record into the overflow series.
    #[must_use]
    pub const fn series_max(mut self, series_max: usize) -> Self {
        self.instrument.series_max = series_max;
        self
    }

    /// The declaration this handle records into.
    #[must_use]
    pub const fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    /// Selects the series the label `values` name, in declaration order.
    pub fn labeled(&self, values: [&'static str; LABELS]) -> HistogramSelection<'_> {
        HistogramSelection {
            instrument: &self.instrument,
            labels: labels(values),
        }
    }
}

impl<const LABELS: usize> Histogram<LABELS> {
    /// Records one duration into the metric values `dispatch` holds, when it holds any;
    /// answers whether it did. A guard dropped on another thread records through the
    /// dispatcher its span was opened under.
    pub(crate) fn record_in(
        &self,
        dispatch: &tracing::Dispatch,
        labels: [&'static str; LABELS],
        elapsed: Duration,
    ) -> bool {
        let Some(layer) = dispatch.downcast_ref::<MetricLayer>() else {
            return false;
        };
        layer.values.observe(
            &self.instrument,
            self::labels(labels),
            elapsed.as_secs_f64(),
        );
        true
    }
}

impl Histogram<0> {
    /// Records one duration.
    pub fn record(&self, elapsed: Duration) {
        self.labeled([]).record(elapsed);
    }
}

/// One series of a histogram, selected by its label values; [`Self::record`] records.
#[derive(Clone, Copy, Debug)]
#[must_use = "a selection records nothing until `record` runs"]
pub struct HistogramSelection<'histogram> {
    instrument: &'histogram Instrument,
    labels: Labels,
}

impl HistogramSelection<'_> {
    /// Records one duration into the selected series, in seconds.
    pub fn record(self, elapsed: Duration) {
        with_installed(|values| {
            values.observe(self.instrument, self.labels, elapsed.as_secs_f64());
        });
    }
}

/// The instruments `rift-tracing` declares and Rift code records into.
#[derive(Debug)]
#[non_exhaustive]
pub struct Metrics {
    /// The process's CPU usage, in percent of one core; past 100 on several cores. The
    /// exported instrument is `process.cpu.utilization`, unit `1`.
    pub cpu: ProcessGauge<f64>,
    /// The process's resident set, in bytes: `process.memory.usage`.
    pub memory: ProcessGauge<u64>,
}

/// The instruments every Rift crate records into.
///
/// ```
/// let metrics = rift_tracing::metrics();
/// metrics.memory.value(4 << 20).record();
/// metrics.cpu.current().record();
/// ```
#[must_use]
pub fn metrics() -> &'static Metrics {
    &METRICS
}

static METRICS: Metrics = Metrics {
    cpu: ProcessGauge::reading(PROCESS_CPU_UTILIZATION, ProcessSample::cpu_percent),
    memory: ProcessGauge::reading(PROCESS_MEMORY_USAGE, ProcessSample::resident_bytes),
};

/// `process.cpu.utilization`: recorded in percent, stored in the unit `1`.
const PROCESS_CPU_UTILIZATION: Gauge<f64, 0> =
    Gauge::declare("process.cpu.utilization", "1", &[]).scaled(0.01);
/// `process.memory.usage`: the resident set in bytes.
const PROCESS_MEMORY_USAGE: Gauge<u64, 0> = Gauge::declare("process.memory.usage", "By", &[]);

/// The duration of every `traced!` operation, the name the OpenTelemetry Collector's span
/// metrics connector derives from spans.
pub(crate) const OPERATION_DURATION: Histogram<3> = Histogram::declare(
    "traces.span.metrics.duration",
    &["span.name", "status.code", "error.type"],
)
.series_max(OPERATION_SERIES_MAX);
/// The count of every `traced!` operation, beside [`OPERATION_DURATION`].
pub(crate) const OPERATION_CALLS: Counter<3> = Counter::declare(
    "traces.span.metrics.calls",
    "{call}",
    &["span.name", "status.code", "error.type"],
)
.series_max(OPERATION_SERIES_MAX);
/// Operation and outcome pairs each operation instrument keeps: every `traced!` literal of
/// the workspace, three outcomes each, with room to spare.
const OPERATION_SERIES_MAX: usize = 1_024;

/// The `status.code` of an operation that finished.
const STATUS_OK: &str = "Ok";
/// The `status.code` of an operation that panicked or was cancelled.
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
}

/// The completion of one `traced!` operation: its duration and its outcome, recorded once
/// when the guard drops.
///
/// The guard reads the monotonic clock only when the thread's dispatcher holds metric
/// values, so a thread recording no metrics pays one dispatcher lookup per operation.
/// Retained clones of the operation's span do not lengthen the recorded duration: the
/// guard drops when the work ends.
#[doc(hidden)]
#[derive(Debug)]
#[must_use = "the completion records when it drops"]
pub struct Completion {
    operation: &'static str,
    started: Option<Duration>,
    ending: Ending,
}

impl Completion {
    /// Marks an awaited operation's work as returned.
    pub(crate) fn finished(&mut self) {
        self.ending = Ending::Finished;
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
        } else if self.ending == Ending::Cancelled {
            (STATUS_ERROR, ERROR_CANCELLED)
        } else {
            (STATUS_OK, "")
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
        started: installed().then(monotonic_now),
        ending,
    }
}

/// The `tracing` layer that carries a dispatcher's metric values, found through it by
/// every handle that records. It observes no span or event.
#[derive(Clone, Debug)]
pub(crate) struct MetricLayer {
    values: Arc<MetricValues>,
}

impl MetricLayer {
    /// The layer over `values`.
    pub(crate) const fn new(values: Arc<MetricValues>) -> Self {
        Self { values }
    }

    /// The metric values the layer carries.
    pub(crate) fn values(&self) -> &MetricValues {
        &self.values
    }
}

impl<S: Subscriber> Layer<S> for MetricLayer {}

#[cfg(test)]
mod tests;
