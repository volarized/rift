use serde_json::Value;

use super::{SNAPSHOT_VALUES_BYTES_MAX, SnapshotRecord, SnapshotSeries, publish};
use crate::metrics::{Counter, Gauge, Histogram, MetricValues};
use crate::{RecordKind, ScopedRecorder};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const CALLS: Counter<2> = Counter::declare(
    "traces.span.metrics.calls",
    "{call}",
    &["span.name", "error.type"],
);
const WAITS: Histogram<2> = Histogram::declare("lock.wait.duration", &["lock.name", "lock.mode"]);
const RESIDENT: Gauge<u64, 0> = Gauge::declare("process.memory.usage", "By", &[]);
const CPU_TIME: Counter<0> = Counter::declare("process.cpu.time", "s", &[]);
const EPOCH: Gauge<u64, 0> = Gauge::declare("index.epoch", "{epoch}", &[]);

/// One record's values, parsed.
fn parsed(record: &SnapshotRecord) -> Result<Value, serde_json::Error> {
    serde_json::from_str(&record.values)
}

/// The record of `group` among `records`.
fn group<'records>(
    records: &'records [SnapshotRecord],
    group: &str,
) -> Option<&'records SnapshotRecord> {
    records.iter().find(|record| record.group == group)
}

#[test]
fn a_tick_where_only_gauges_and_process_counters_moved_publishes_nothing() {
    let values = MetricValues::default();
    RESIDENT.record_into(&values, [], 4 << 20);
    CPU_TIME.add_into(&values, [], 0.25);
    EPOCH.record_into(&values, [], 3);
    let mut series = SnapshotSeries::default();

    assert!(series.records(&values.snapshot(), false).is_empty());
}

#[test]
fn a_tick_where_only_runtime_counters_moved_publishes_nothing() {
    const BUSY: Counter<0> = Counter::declare("tokio.runtime.worker.busy.time", "s", &[]);
    const PARKS: Counter<0> = Counter::declare("tokio.runtime.worker.parks", "{park}", &[]);
    let values = MetricValues::default();
    BUSY.add_into(&values, [], 0.01);
    PARKS.add_into(&values, [], 2.0);
    let mut series = SnapshotSeries::default();

    assert!(
        series.records(&values.snapshot(), false).is_empty(),
        "the sampler's own tick moves the runtime counters"
    );
    BUSY.add_into(&values, [], 0.02);
    let forced = series.records(&values.snapshot(), true);
    let runtime = group(&forced, "runtime").expect("a forced tick publishes the runtime group");
    assert!(runtime.values.contains("tokio.runtime.worker.busy.time"));
}

#[test]
fn a_counter_prints_its_change_since_the_previous_snapshot() -> TestResult {
    let values = MetricValues::default();
    let mut series = SnapshotSeries::default();
    CALLS.add_into(&values, ["lexical.commit", ""], 2.0);
    RESIDENT.record_into(&values, [], 100);
    let first = series.records(&values.snapshot(), false);
    CALLS.add_into(&values, ["lexical.commit", ""], 3.0);
    let second = series.records(&values.snapshot(), false);
    let third = series.records(&values.snapshot(), false);

    let key = "traces.span.metrics.calls{span.name=lexical.commit}";
    let operations = group(&first, "operations").ok_or("the operations group")?;
    assert_eq!(parsed(operations)?[key], 2.0);
    let process = group(&first, "process").ok_or("the process group rides along")?;
    assert_eq!(parsed(process)?["process.memory.usage"], 100.0);
    assert_eq!(
        parsed(group(&second, "operations").ok_or("moved again")?)?[key],
        3.0
    );
    assert!(third.is_empty(), "nothing moved since the second snapshot");
    Ok(())
}

#[test]
fn a_histogram_prints_its_count_and_sum_changes_under_its_labels() -> TestResult {
    let values = MetricValues::default();
    let mut series = SnapshotSeries::default();
    values.observe(
        WAITS.instrument(),
        ["index.write", "exclusive", "", ""],
        0.5,
    );
    values.observe(
        WAITS.instrument(),
        ["index.write", "exclusive", "", ""],
        0.25,
    );
    let records = series.records(&values.snapshot(), false);

    let locks = values_of(&records, "locks")?;
    assert_eq!(
        locks["lock.wait.duration.count{lock.name=index.write,lock.mode=exclusive}"],
        2.0
    );
    assert_eq!(
        locks["lock.wait.duration.sum{lock.name=index.write,lock.mode=exclusive}"],
        0.75
    );
    Ok(())
}

/// The values of the record of `name`, parsed.
fn values_of(records: &[SnapshotRecord], name: &str) -> Result<Value, Box<dyn std::error::Error>> {
    Ok(parsed(
        group(records, name).ok_or("the group is published")?,
    )?)
}

#[test]
fn an_overflow_series_and_an_empty_label_print_as_declared() -> TestResult {
    const BOUNDED: Counter<2> = Counter::declare(
        "traces.span.metrics.calls",
        "{call}",
        &["span.name", "error.type"],
    )
    .series_max(1);
    let values = MetricValues::default();
    BOUNDED.add_into(&values, ["index.parse", ""], 1.0);
    BOUNDED.add_into(&values, ["index.map", "cancelled"], 1.0);
    let records = SnapshotSeries::default().records(&values.snapshot(), false);

    let operations = values_of(&records, "operations")?;
    assert_eq!(
        operations["traces.span.metrics.calls{span.name=index.parse}"],
        1.0
    );
    assert_eq!(operations["traces.span.metrics.calls{overflow}"], 1.0);
    Ok(())
}

#[test]
fn a_whole_value_prints_without_a_fraction() {
    assert_eq!(super::printed_number(2.0).to_string(), "2");
    assert_eq!(super::printed_number(-3.0).to_string(), "-3");
    assert_eq!(super::printed_number(0.75).to_string(), "0.75");
    assert_eq!(super::printed_number(f64::NAN), Value::Null);
    assert_eq!(super::printed_number(1e300).to_string(), "1e+300");
}

#[test]
fn a_forced_tick_publishes_without_movement() {
    let values = MetricValues::default();
    RESIDENT.record_into(&values, [], 100);
    let records = SnapshotSeries::default().records(&values.snapshot(), true);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].group, "process");
}

#[test]
fn a_group_past_the_field_bound_keeps_whole_members_and_counts_the_rest() -> TestResult {
    const MANY: Counter<1> =
        Counter::declare("traces.span.metrics.calls", "{call}", &["span.name"]).series_max(1_024);
    let values = MetricValues::default();
    let leaked: Vec<&'static str> = (0..400)
        .map(|index| {
            &*Box::leak(format!("lexical.documents.with.a.long.name.{index}").into_boxed_str())
        })
        .collect();
    for name in &leaked {
        MANY.add_into(&values, [*name], 1.0);
    }
    let records = SnapshotSeries::default().records(&values.snapshot(), false);

    let operations = group(&records, "operations").ok_or("published")?;
    assert!(operations.values.len() <= SNAPSHOT_VALUES_BYTES_MAX + 64);
    let parsed = parsed(operations)?;
    let kept = parsed.as_object().map_or(0, serde_json::Map::len) as u64 - 1;
    assert_eq!(
        parsed["series_left_out"].as_u64().map(|left| left + kept),
        Some(400)
    );
    Ok(())
}

#[test]
fn a_published_snapshot_is_a_metric_record_the_capture_filter_selects() -> TestResult {
    let records = vec![SnapshotRecord {
        group: "locks",
        values: "{\"lock.wait.duration.count{lock.name=index.write}\":2}".to_owned(),
    }];
    let (recorder, mut drain) = ScopedRecorder::builder().install()?;
    publish(&records);
    drop(recorder);
    let (filtered, mut nothing) = ScopedRecorder::builder()
        .capture("info,rift_tracing::metric=off")
        .install()?;
    publish(&records);
    drop(filtered);

    let captured = drain.queued_records();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].kind(), RecordKind::Metric);
    assert_eq!(captured[0].target(), "rift_tracing::metric");
    assert_eq!(captured[0].operation(), "locks");
    assert_eq!(captured[0].message(), "metric snapshot");
    assert_eq!(captured[0].fields(), records[0].values);
    assert!(nothing.queued_records().is_empty());
    Ok(())
}
