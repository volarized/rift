//! Metric snapshot records: the values of the process's instruments, written into the log
//! stream on the sampler tick.
//!
//! A log record and a metric share no clock in process: the store keeps `recorded_at` and
//! the instruments keep running sums. A snapshot record carries the instrument values on
//! the log clock, under one identity order with the log records around it, so a read of a
//! window places what the metrics said beside what the code logged.
//!
//! One tick publishes one record per instrument group: `operations`, `locks`, `database`,
//! `runtime`, `process`, and `lifecycle` for every other instrument. A gauge carries its
//! value; a counter carries its change since the previous snapshot; a histogram carries the
//! change of its count and of its sum. A tick at which no counter or histogram outside the
//! `process` and `runtime` groups moved publishes nothing, so an idle server writes no
//! snapshots: the sampler's own tick moves the process's CPU time and the runtime's busy
//! time and parks.
//!
//! A series prints under its instrument name and, when it has labels, the label values in
//! declaration order inside braces, a label recorded empty left out:
//! `lock.wait.duration.count{lock.name=index.write,lock.mode=exclusive}`. A histogram
//! prints two members, `<name>.count` and `<name>.sum`. The overflow series of an
//! instrument prints as `<name>{overflow}`.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde_json::{Map, Number, Value};

use crate::metrics::{MetricSeries, MetricSnapshot, SeriesValue};
use crate::record::LOG_FIELDS_BYTES_MAX;

/// The target every metric snapshot record carries; `RUST_LOG` and `[logs] capture`
/// select snapshots through it.
pub(crate) const METRIC_TARGET: &str = "rift_tracing::metric";
/// The event field that carries a snapshot record's values.
pub(crate) const SNAPSHOT_VALUES_FIELD: &str = "values";
/// Bytes of one snapshot record's values, at most: the members that fit are kept whole, and
/// the count of the rest follows as `series_left_out`.
const SNAPSHOT_VALUES_BYTES_MAX: usize = LOG_FIELDS_BYTES_MAX - 64;
/// The group of the process sampler's instruments.
const PROCESS_GROUP: &str = "process";
/// The group of the Tokio runtime's instruments.
const RUNTIME_GROUP: &str = "runtime";
/// The groups whose movement alone publishes nothing: every tick moves them.
const SAMPLED_GROUPS: [&str; 2] = [PROCESS_GROUP, RUNTIME_GROUP];

/// The instrument group a series belongs to, by its instrument name.
fn group(name: &str) -> &'static str {
    let prefixed = |prefix: &str| name.starts_with(prefix);
    if prefixed("traces.span.metrics.") {
        "operations"
    } else if prefixed("lock.") {
        "locks"
    } else if prefixed("sqlite.") || prefixed("db.") {
        "database"
    } else if prefixed("tokio.") {
        RUNTIME_GROUP
    } else if prefixed("process.") {
        PROCESS_GROUP
    } else {
        "lifecycle"
    }
}

/// The printed key of `series` with `suffix` after its instrument name.
fn series_key(series: &MetricSeries, suffix: &str) -> String {
    let mut key = format!("{}{suffix}", series.name());
    if series.is_overflow() {
        key.push_str("{overflow}");
        return key;
    }
    if series.labels().is_empty() {
        return key;
    }
    key.push('{');
    for (index, (label, value)) in series.labels().iter().enumerate() {
        if index > 0 {
            key.push(',');
        }
        let _ = write!(key, "{label}={value}");
    }
    key.push('}');
    key
}

/// One group's record: its name and its values as a JSON object.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SnapshotRecord {
    pub(crate) group: &'static str,
    pub(crate) values: String,
}

/// What the previous snapshot read, by printed key: the sums counters and histograms had.
#[derive(Debug, Default)]
pub(crate) struct SnapshotSeries {
    previous: HashMap<String, f64>,
}

impl SnapshotSeries {
    /// The records of `snapshot`: changes since the previous call for counters and
    /// histograms, values for gauges. `forced` publishes even when nothing moved, for a
    /// tick that reports a stall.
    pub(crate) fn records(
        &mut self,
        snapshot: &MetricSnapshot,
        forced: bool,
    ) -> Vec<SnapshotRecord> {
        let mut groups: Vec<(&'static str, Vec<(String, f64)>)> = Vec::new();
        let mut moved = false;
        for series in snapshot.series() {
            let group = group(series.name());
            let mut members = Vec::new();
            match series.value() {
                SeriesValue::Last(value) => members.push((series_key(series, ""), *value)),
                SeriesValue::Sum(sum) => {
                    let change = self.change(series_key(series, ""), *sum);
                    members.extend(change);
                }
                SeriesValue::Buckets { count, sum, .. } => {
                    #[expect(
                        clippy::cast_precision_loss,
                        reason = "a histogram count past 2^53 values is not one interval's"
                    )]
                    let count = *count as f64;
                    members.extend(self.change(series_key(series, ".count"), count));
                    members.extend(self.change(series_key(series, ".sum"), *sum));
                }
            }
            if !SAMPLED_GROUPS.contains(&group)
                && !matches!(series.value(), SeriesValue::Last(_))
                && !members.is_empty()
            {
                moved = true;
            }
            if members.is_empty() {
                continue;
            }
            match groups.iter_mut().find(|(name, _)| *name == group) {
                Some((_, held)) => held.extend(members),
                None => groups.push((group, members)),
            }
        }
        if !moved && !forced {
            return Vec::new();
        }
        groups
            .into_iter()
            .map(|(group, members)| SnapshotRecord {
                group,
                values: bounded_values(members),
            })
            .collect()
    }

    /// The change of the running sum `key` since the previous snapshot, when it moved.
    fn change(&mut self, key: String, sum: f64) -> Option<(String, f64)> {
        let previous = self.previous.insert(key.clone(), sum).unwrap_or(0.0);
        let change = sum - previous;
        (change != 0.0).then_some((key, change))
    }
}

/// `members` as one JSON object of at most [`SNAPSHOT_VALUES_BYTES_MAX`] bytes, members
/// kept whole in order, then `series_left_out` when any did not fit.
fn bounded_values(members: Vec<(String, f64)>) -> String {
    let mut values = String::from("{");
    let mut left_out = 0_u64;
    for (key, value) in members {
        let number = printed_number(value);
        let member = format!("{}:{number}", Value::from(key));
        let separator = usize::from(values.len() > 1);
        if values.len() + separator + member.len() + 1 > SNAPSHOT_VALUES_BYTES_MAX {
            left_out += 1;
            continue;
        }
        if separator == 1 {
            values.push(',');
        }
        values.push_str(&member);
    }
    if left_out > 0 {
        let mut tail = Map::new();
        tail.insert("series_left_out".to_owned(), Value::from(left_out));
        let tail = Value::Object(tail).to_string();
        if values.len() > 1 {
            values.push(',');
        }
        values.push_str(&tail[1..tail.len() - 1]);
    }
    values.push('}');
    values
}

/// Largest magnitude below which every integer is an exact `f64`: 2^53.
const EXACT_INTEGER_MAX: f64 = 9_007_199_254_740_992.0;

/// `value` as a JSON number: a whole count prints without a fraction, as `2` and not `2.0`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "a whole value below 2^53 converts to i64 exactly"
)]
fn printed_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < EXACT_INTEGER_MAX {
        return Value::from(value as i64);
    }
    Number::from_f64(value).map_or(Value::Null, Value::Number)
}

/// Publishes `records` as metric snapshot records through the current dispatcher.
pub(crate) fn publish(records: &[SnapshotRecord]) {
    for record in records {
        tracing::info!(
            target: "rift_tracing::metric",
            operation = record.group,
            values = %record.values,
            "metric snapshot"
        );
    }
}

#[cfg(test)]
mod tests;
