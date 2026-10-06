//! Elapsed time a caller acts on: one operation's measurement and the clock behind it.

use std::fmt;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Reads the monotonic clock as the time since its first read.
///
/// The epoch is fixed on the first read, so two readings subtract to the
/// elapsed time between them. On a thread whose default subscriber is a
/// [`ScopedRecorder`](crate::ScopedRecorder) built with
/// [`clock`](crate::ScopedRecorderBuilder::clock), the reading is that clock's; a read
/// made from inside a subscriber callback, where `tracing` hands out no default, reads
/// the process clock.
#[doc(hidden)]
#[must_use]
pub fn monotonic_now() -> Duration {
    #[cfg(any(test, feature = "fixtures"))]
    if let Some(now) = crate::capture::scoped_now() {
        return now;
    }
    process_monotonic_now()
}

/// Reads the process-local monotonic clock as the time since its first read, whatever
/// clock a scoped recorder named.
pub(crate) fn process_monotonic_now() -> Duration {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed()
}

/// Clock moved backwards during one measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockRegression {
    start: Duration,
    finish: Duration,
}

impl ClockRegression {
    /// Returns start tick.
    #[must_use]
    pub const fn start(self) -> Duration {
        self.start
    }

    /// Returns regressed finish tick.
    #[must_use]
    pub const fn finish(self) -> Duration {
        self.finish
    }
}

impl fmt::Display for ClockRegression {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "monotonic clock regressed from {:?} to {:?}",
            self.start, self.finish
        )
    }
}

impl std::error::Error for ClockRegression {}

/// Completed operation measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerformanceMeasurement {
    operation: &'static str,
    elapsed: Duration,
}

impl PerformanceMeasurement {
    /// Constructs measurement from monotonic clock ticks.
    ///
    /// # Errors
    ///
    /// Returns [`ClockRegression`] when finish precedes start.
    pub fn between(
        operation: &'static str,
        start: Duration,
        finish: Duration,
    ) -> Result<Self, ClockRegression> {
        let elapsed = finish
            .checked_sub(start)
            .ok_or(ClockRegression { start, finish })?;
        Ok(Self { operation, elapsed })
    }

    /// Returns stable operation name.
    #[must_use]
    pub const fn operation(self) -> &'static str {
        self.operation
    }

    /// Returns measured elapsed duration.
    #[must_use]
    pub const fn elapsed(self) -> Duration {
        self.elapsed
    }
}

/// Evaluates an expression or block once and returns its value with the elapsed time.
///
/// The macro reads the monotonic clock before and after the
/// body and returns `Result<(value, PerformanceMeasurement), ClockRegression>`.
/// The operation name is a string literal. The body is inlined, so `?`,
/// `return`, `break`, and `continue` inside it act on the enclosing function or
/// loop, and the clock is not read a second time on those paths.
///
/// ```
/// let (sum, measurement) = rift_tracing::measure_elapsed!("index.parse", 1 + 1)
///     .expect("the monotonic clock does not regress");
/// assert_eq!(sum, 2);
/// assert_eq!(measurement.operation(), "index.parse");
///
/// let (scanned, _) = rift_tracing::measure_elapsed!("index.scan", {
///     let sources = 3;
///     sources * 2
/// })
/// .expect("the monotonic clock does not regress");
/// assert_eq!(scanned, 6);
/// ```
///
/// A computed name is refused at compile time:
///
/// ```compile_fail
/// let name = "index.parse";
/// let _ = rift_tracing::measure_elapsed!(name, 1 + 1);
/// ```
#[macro_export]
macro_rules! measure_elapsed {
    ($operation:literal, $body:expr $(,)?) => {{
        let __rift_started = $crate::__private::monotonic_now();
        let __rift_value = $body;
        let __rift_finished = $crate::__private::monotonic_now();
        $crate::PerformanceMeasurement::between($operation, __rift_started, __rift_finished)
            .map(|__rift_measurement| (__rift_value, __rift_measurement))
    }};
}

#[cfg(test)]
mod tests {
    use super::{PerformanceMeasurement, monotonic_now};
    use std::cell::Cell;
    use std::time::Duration;

    #[test]
    fn macro_evaluates_body_once_and_returns_value_unchanged() {
        let calls = Cell::new(0_u32);
        let (value, measurement) = measure_elapsed!("search", {
            calls.set(calls.get() + 1);
            42
        })
        .expect("the monotonic clock does not regress");

        assert_eq!((value, measurement.operation()), (42, "search"));
        assert_eq!(calls.get(), 1);
    }

    fn halve(value: u32) -> Result<u32, &'static str> {
        let (halved, _) = measure_elapsed!("halve", {
            if value % 2 == 1 {
                return Err("odd");
            }
            value / 2
        })
        .map_err(|_| "regressed")?;
        Ok(halved)
    }

    #[test]
    fn macro_return_leaves_the_enclosing_function() {
        assert_eq!(halve(8), Ok(4));
        assert_eq!(halve(7), Err("odd"));
    }

    #[test]
    fn regression_is_reported() {
        let error = PerformanceMeasurement::between(
            "index",
            Duration::from_millis(8),
            Duration::from_millis(3),
        )
        .expect_err("finish before start must report clock regression");

        assert_eq!(error.start(), Duration::from_millis(8));
        assert_eq!(error.finish(), Duration::from_millis(3));
        assert_eq!(
            error.to_string(),
            "monotonic clock regressed from 8ms to 3ms"
        );
    }

    #[test]
    fn monotonic_clock_does_not_regress() {
        let start = monotonic_now();
        let finish = monotonic_now();

        assert!(finish >= start);
    }
}
