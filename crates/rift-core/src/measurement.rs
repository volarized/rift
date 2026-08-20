use std::fmt;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Monotonic clock suitable for elapsed-time measurement.
pub trait MonotonicClock {
    /// Returns monotonically increasing duration from clock epoch.
    fn now(&self) -> Duration;
}

/// Process-local monotonic clock backed by [`Instant`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemMonotonicClock;

impl MonotonicClock for SystemMonotonicClock {
    fn now(&self) -> Duration {
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        EPOCH.get_or_init(Instant::now).elapsed()
    }
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

#[cfg(test)]
mod tests {
    use super::{MonotonicClock, PerformanceMeasurement, SystemMonotonicClock};
    use crate::measure_elapsed;
    use std::cell::Cell;
    use std::time::Duration;

    #[derive(Debug)]
    struct FakeClock {
        calls: Cell<usize>,
        ticks: [Duration; 2],
    }

    impl MonotonicClock for FakeClock {
        fn now(&self) -> Duration {
            let call = self.calls.get();
            self.calls.set(call + 1);
            self.ticks[call]
        }
    }

    #[test]
    fn macro_measures_block_once_with_fake_clock() {
        let clock = FakeClock {
            calls: Cell::new(0),
            ticks: [Duration::from_millis(10), Duration::from_millis(17)],
        };
        let body_calls = Cell::new(0);
        let measured = measure_elapsed!(clock, "search", {
            body_calls.set(body_calls.get() + 1);
            42
        });

        assert_eq!(
            measured.map(|(value, measurement)| (
                value,
                measurement.operation(),
                measurement.elapsed()
            )),
            Ok((42, "search", Duration::from_millis(7)))
        );
        assert_eq!(body_calls.get(), 1);
        assert_eq!(clock.calls.get(), 2);
    }

    #[test]
    fn regression_is_reported() {
        let result = PerformanceMeasurement::between(
            "index",
            Duration::from_millis(8),
            Duration::from_millis(3),
        );

        let Err(error) = result else {
            panic!("finish before start must report clock regression");
        };
        assert_eq!(error.start(), Duration::from_millis(8));
        assert_eq!(error.finish(), Duration::from_millis(3));
        assert_eq!(
            error.to_string(),
            "monotonic clock regressed from 8ms to 3ms"
        );
    }

    #[test]
    fn system_clock_does_not_regress() {
        let clock = SystemMonotonicClock;
        let start = clock.now();
        let finish = clock.now();

        assert!(finish >= start);
    }
}
