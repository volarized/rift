//! Bounded retry and restart policies an operator states per engine.
//!
//! [`RetryPolicy`] answers one question: given the attempts already made,
//! how long to wait before the next one. It computes; it never sleeps, so
//! the loop that does is a thin shell over it. [`RestartPolicy`] bounds how
//! often Rift may replace one language engine, and over what window, so a
//! crash-looping engine cannot be restarted forever. Between them they
//! bound every wait Rift takes on one engine before it reports.
//!
//! Both are configuration models: they deserialize from `rift.toml`,
//! advertise their ranges to the exported schema, and are compared field by
//! field when the server decides whether a reloaded file changed anything.

use std::time::Duration as Elapsed;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::configuration::Duration;

/// Attempts one retried operation may take, at least: no resend.
pub const RETRY_ATTEMPTS_MIN: u64 = 1;
/// Attempts one retried operation may take, at most.
pub const RETRY_ATTEMPTS_MAX: u64 = 64;
/// Attempts `retry.attempts` holds when the key is absent.
const RETRY_ATTEMPTS_DEFAULT: u64 = 8;
/// Milliseconds one wait between attempts may hold, at least.
pub const RETRY_DELAY_MS_MIN: u64 = 1;
/// Milliseconds `retry.delay` may hold, at most: one minute.
pub const RETRY_DELAY_MS_MAX: u64 = 60_000;
/// Milliseconds `retry.delay` holds when the key is absent.
const RETRY_DELAY_MS_DEFAULT: u64 = 250;
/// Milliseconds `retry.delay_limit` may hold, at least.
pub const RETRY_DELAY_LIMIT_MS_MIN: u64 = 1;
/// Milliseconds `retry.delay_limit` may hold, at most: ten minutes.
pub const RETRY_DELAY_LIMIT_MS_MAX: u64 = 600_000;
/// Milliseconds `retry.delay_limit` holds when the key is absent.
const RETRY_DELAY_LIMIT_MS_DEFAULT: u64 = 2_000;
/// The factor each attempt multiplies the previous wait by.
///
/// Growth is not an operator key: `delay` and `delay_limit` already place
/// the curve, and a third key would only let one flatten it into the fixed
/// interval `delay_limit` states more directly.
pub const RETRY_GROWTH_FACTOR: u64 = 2;

/// Restarts one engine may take inside its window, at least: none.
pub const RESTART_ATTEMPTS_MIN: u64 = 0;
/// Restarts one engine may take inside its window, at most.
pub const RESTART_ATTEMPTS_MAX: u64 = 16;
/// Restarts `restart.attempts` holds when the key is absent.
const RESTART_ATTEMPTS_DEFAULT: u64 = 3;
/// Milliseconds one engine's restarts are counted over, at least: one
/// second.
pub const RESTART_WINDOW_MS_MIN: u64 = 1_000;
/// Milliseconds one engine's restarts are counted over, at most: one day.
pub const RESTART_WINDOW_MS_MAX: u64 = 86_400_000;
/// Milliseconds `restart.window` holds when the key is absent.
const RESTART_WINDOW_MS_DEFAULT: u64 = 300_000;

/// How often one unsettled operation is attempted again, and how the
/// waits between attempts grow.
///
/// Attempts are numbered from one and the first is counted, so
/// `attempts = 1` never resends. Each wait is twice the one before it,
/// held at `delay_limit`; a `delay_limit` below `delay` therefore holds
/// every wait at `delay_limit`.
///
/// The waits carry no jitter. Jitter spreads a set of independent retriers
/// apart so they stop arriving together; requests to one language engine
/// serialize on that engine's own slot, so there is never such a set, and
/// the sequence stays a function of the attempt number alone.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_retry_ranges)]
pub struct RetryPolicy {
    /// Attempts one operation takes, the first counted, 1 to 64.
    #[serde(default = "default_retry_attempts")]
    #[schemars(range(min = 1, max = 64))]
    pub attempts: u64,
    /// Wait before the second attempt, 1ms to 1m. Every later wait
    /// doubles the one before it.
    #[serde(default = "default_retry_delay")]
    pub delay: Duration,
    /// Longest wait between two attempts, 1ms to 10m.
    #[serde(default = "default_retry_delay_limit")]
    pub delay_limit: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: RETRY_ATTEMPTS_DEFAULT,
            delay: default_retry_delay(),
            delay_limit: default_retry_delay_limit(),
        }
    }
}

impl RetryPolicy {
    /// The wait before the attempt after `attempt`, absent once the
    /// attempt bound is spent.
    ///
    /// Attempts are numbered from one, so `delay_after(1)` is the wait
    /// before the second attempt and answers `delay`. The wait is `delay`
    /// times [`RETRY_GROWTH_FACTOR`] raised to the attempts already made,
    /// held at `delay_limit`. Growth that would overflow saturates, and
    /// the ceiling clamps the saturated value, so an unvalidated attempt
    /// bound cannot produce a nonsense wait.
    ///
    /// # Examples
    ///
    /// ```
    /// use rift_protocol::retry::RetryPolicy;
    ///
    /// let policy = RetryPolicy::default();
    /// assert_eq!(policy.delay_after(1).map(|wait| wait.as_millis()), Some(250));
    /// assert_eq!(policy.delay_after(2).map(|wait| wait.as_millis()), Some(500));
    /// assert_eq!(policy.delay_after(8), None);
    /// ```
    #[must_use]
    pub fn delay_after(&self, attempt: u64) -> Option<Elapsed> {
        if attempt >= self.attempts {
            return None;
        }
        let made = attempt.saturating_sub(1);
        let growth = u32::try_from(made)
            .ok()
            .and_then(|exponent| RETRY_GROWTH_FACTOR.checked_pow(exponent));
        let grown = growth
            .and_then(|growth| self.delay.milliseconds().checked_mul(growth))
            .unwrap_or(u64::MAX);
        Some(Elapsed::from_millis(
            grown.min(self.delay_limit.milliseconds()),
        ))
    }
}

/// How often Rift may replace one language engine on its own, and over
/// what window.
///
/// A restart is any start after the first: the first is the start, and
/// every start that follows it replaces an engine that ended, failed to
/// start, or stopped answering. A restart older than `window` no longer
/// counts, so a workspace whose engine dies once a day keeps its full
/// budget while a crash-looping one spends it and stops.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
#[schemars(transform = crate::schema::declare_restart_ranges)]
pub struct RestartPolicy {
    /// Restarts allowed inside one window, 0 to 16. Zero never restarts.
    #[serde(default = "default_restart_attempts")]
    #[schemars(range(min = 0, max = 16))]
    pub attempts: u64,
    /// Span the restarts are counted over, 1s to 1d.
    #[serde(default = "default_restart_window")]
    pub window: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            attempts: RESTART_ATTEMPTS_DEFAULT,
            window: default_restart_window(),
        }
    }
}

impl RestartPolicy {
    /// The window as an elapsed span, for comparing against a clock.
    #[must_use]
    pub const fn window(&self) -> Elapsed {
        Elapsed::from_millis(self.window.milliseconds())
    }
}

fn default_retry_attempts() -> u64 {
    RETRY_ATTEMPTS_DEFAULT
}

fn default_retry_delay() -> Duration {
    Duration::from_millis(RETRY_DELAY_MS_DEFAULT)
}

fn default_retry_delay_limit() -> Duration {
    Duration::from_millis(RETRY_DELAY_LIMIT_MS_DEFAULT)
}

fn default_restart_attempts() -> u64 {
    RESTART_ATTEMPTS_DEFAULT
}

fn default_restart_window() -> Duration {
    Duration::from_millis(RESTART_WINDOW_MS_DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy(attempts: u64, delay_ms: u64, limit_ms: u64) -> RetryPolicy {
        RetryPolicy {
            attempts,
            delay: Duration::from_millis(delay_ms),
            delay_limit: Duration::from_millis(limit_ms),
        }
    }

    fn waits(policy: &RetryPolicy) -> Vec<Option<u64>> {
        (1..=policy.attempts)
            .map(|attempt| {
                policy
                    .delay_after(attempt)
                    .map(|wait| u64::try_from(wait.as_millis()).unwrap_or(u64::MAX))
            })
            .collect()
    }

    #[test]
    fn test_delay_sequence_doubles_from_the_base_delay() {
        let sequence = waits(&policy(5, 100, 60_000));
        assert_eq!(
            sequence,
            vec![Some(100), Some(200), Some(400), Some(800), None],
            "each wait doubles the one before it until the bound is spent"
        );
    }

    #[test]
    fn test_delay_holds_at_the_ceiling_once_growth_passes_it() {
        let sequence = waits(&policy(6, 100, 350));
        assert_eq!(
            sequence,
            vec![Some(100), Some(200), Some(350), Some(350), Some(350), None],
            "the ceiling clamps every wait that grew past it"
        );
    }

    #[test]
    fn test_a_ceiling_below_the_base_delay_holds_every_wait_at_the_ceiling() {
        let sequence = waits(&policy(3, 5_000, 1_000));
        assert_eq!(sequence, vec![Some(1_000), Some(1_000), None]);
    }

    #[test]
    fn test_the_attempt_bound_is_the_exhausted_verdict() {
        let single = policy(1, 100, 1_000);
        assert_eq!(single.delay_after(1), None, "one attempt never resends");
        let paired = policy(2, 100, 1_000);
        assert_eq!(paired.delay_after(1), Some(Elapsed::from_millis(100)));
        assert_eq!(paired.delay_after(2), None);
        assert_eq!(
            paired.delay_after(u64::MAX),
            None,
            "an attempt past the bound stays exhausted"
        );
    }

    #[test]
    fn test_growth_that_would_overflow_saturates_into_the_ceiling() {
        let wide = policy(u64::MAX, u64::MAX, 30_000);
        assert_eq!(
            wide.delay_after(64),
            Some(Elapsed::from_secs(30)),
            "an exponent past a u32 saturates and the ceiling clamps it"
        );
        assert_eq!(
            wide.delay_after(40),
            Some(Elapsed::from_secs(30)),
            "a product past a u64 saturates and the ceiling clamps it"
        );
    }

    #[test]
    fn test_defaults_carry_the_shipped_engine_pacing() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.attempts, 8);
        assert_eq!(policy.delay, Duration::from_millis(250));
        assert_eq!(policy.delay_limit, Duration::from_millis(2_000));
        let total: u128 = (1..policy.attempts)
            .filter_map(|attempt| policy.delay_after(attempt))
            .map(|wait| wait.as_millis())
            .sum();
        assert_eq!(total, 9_750, "the eight attempts add at most 9.75s");
    }

    #[test]
    fn test_restart_defaults_and_window_conversion() {
        let policy = RestartPolicy::default();
        assert_eq!(policy.attempts, 3);
        assert_eq!(policy.window, Duration::from_millis(300_000));
        assert_eq!(policy.window(), Elapsed::from_mins(5));
    }

    #[test]
    fn test_absent_keys_fall_to_the_defaults_and_present_ones_win() {
        let empty: RetryPolicy = serde_json::from_value(json!({})).expect("an empty table decodes");
        assert_eq!(empty, RetryPolicy::default());
        let partial: RetryPolicy =
            serde_json::from_value(json!({ "delay": "1s" })).expect("a partial table decodes");
        assert_eq!(partial.delay, Duration::from_millis(1_000));
        assert_eq!(partial.attempts, RetryPolicy::default().attempts);
        let restart: RestartPolicy =
            serde_json::from_value(json!({ "attempts": 0 })).expect("a partial table decodes");
        assert_eq!(restart.attempts, 0);
        assert_eq!(restart.window, RestartPolicy::default().window);
    }

    #[test]
    fn test_policies_round_trip_through_json_with_exact_wire_names() {
        let value = serde_json::to_value(RetryPolicy::default()).expect("serialize");
        assert_eq!(
            value,
            json!({ "attempts": 8, "delay": "250ms", "delay_limit": "2s" })
        );
        let value = serde_json::to_value(RestartPolicy::default()).expect("serialize");
        assert_eq!(value, json!({ "attempts": 3, "window": "5m" }));
        let unknown = serde_json::from_value::<RetryPolicy>(json!({ "jitter": true }));
        assert!(unknown.is_err(), "an unknown key refuses the table");
    }

    #[test]
    fn test_schema_bounds_and_defaults_equal_the_enforced_constants() {
        let retry = serde_json::to_value(schemars::schema_for!(RetryPolicy)).expect("schema");
        let keys = &retry["properties"];
        assert_eq!(keys["attempts"]["minimum"], json!(RETRY_ATTEMPTS_MIN));
        assert_eq!(keys["attempts"]["maximum"], json!(RETRY_ATTEMPTS_MAX));
        assert_eq!(keys["attempts"]["default"], json!(RETRY_ATTEMPTS_DEFAULT));
        assert_eq!(keys["delay"]["default"], json!("250ms"));
        assert_eq!(keys["delay_limit"]["default"], json!("2s"));
        assert_eq!(
            keys["delay"]["rift:range"],
            json!({ "min": "1ms", "max": "1m" })
        );
        assert_eq!(
            keys["delay_limit"]["rift:range"],
            json!({ "min": "1ms", "max": "10m" })
        );
        let restart = serde_json::to_value(schemars::schema_for!(RestartPolicy)).expect("schema");
        let keys = &restart["properties"];
        assert_eq!(keys["attempts"]["minimum"], json!(RESTART_ATTEMPTS_MIN));
        assert_eq!(keys["attempts"]["maximum"], json!(RESTART_ATTEMPTS_MAX));
        assert_eq!(keys["attempts"]["default"], json!(RESTART_ATTEMPTS_DEFAULT));
        assert_eq!(keys["window"]["default"], json!("5m"));
        assert_eq!(
            keys["window"]["rift:range"],
            json!({ "min": "1s", "max": "1d" })
        );
    }
}
