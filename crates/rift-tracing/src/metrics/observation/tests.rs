use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use opentelemetry::{KeyValue, metrics::AsyncInstrument};

use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use crate::metrics::{ObservableUpDownCounter, SCOPE, cardinality_view, install_meter};
use crate::recorder::ScopedRecorder;

static HELD: ObservableUpDownCounter<2> = ObservableUpDownCounter::declare(
    SCOPE,
    "test.observation.held",
    "{item}",
    &["test.kind", "test.state"],
);

#[derive(Default)]
struct CountedObservations(AtomicUsize);

impl AsyncInstrument<i64> for CountedObservations {
    fn observe(&self, _measurement: i64, _attributes: &[KeyValue]) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn points(exporter: &InMemoryMetricExporter) -> BTreeMap<Vec<(String, String)>, i64> {
    let finished = exporter.get_finished_metrics().expect("an export");
    let Some(finished) = finished.last() else {
        return BTreeMap::new();
    };
    let metric = finished
        .scope_metrics()
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .find(|metric| metric.name() == "test.observation.held");
    let Some(metric) = metric else {
        return BTreeMap::new();
    };
    let AggregatedMetrics::I64(MetricData::Sum(sum)) = metric.data() else {
        panic!("an observable counter exports an i64 sum");
    };
    sum.data_points()
        .map(|point| {
            let mut labels: Vec<_> = point
                .attributes()
                .map(|pair| (pair.key.as_str().to_owned(), pair.value.to_string()))
                .collect();
            labels.sort();
            (labels, point.value())
        })
        .collect()
}

#[test]
fn test_observation_sdk_sums_owners_overflow_and_removes_stale_series() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .with_view(cardinality_view(2))
        .build();
    install_meter(provider.clone(), 2);
    let discarded = SdkMeterProvider::builder()
        .with_view(cardinality_view(1))
        .build();
    install_meter(discarded, 1);
    let (recorder, mut drain) = ScopedRecorder::builder().install().expect("recorder");
    let first = HELD
        .observe(|observation| {
            observation.observe(["parse", "ready"], 2);
            observation.observe(["ready", "parse"], 7);
            observation.observe(["", ""], 11);
            observation.observe(["extra", "ready"], 13);
        })
        .expect("first owner");
    let second = HELD
        .observe(|observation| {
            observation.observe(["parse", "ready"], 3);
            observation.observe(["another", "ready"], 17);
        })
        .expect("second owner");
    let counted = CountedObservations::default();
    HELD.collect(&counted);
    HELD.collect(&counted);
    assert_eq!(counted.0.load(Ordering::Relaxed), 8);
    provider.force_flush().expect("the provider flushes");
    let observed = points(&exporter);
    let mut values: Vec<_> = observed.values().copied().collect();
    values.sort_unstable();
    assert_eq!(values, [5, 7, 11, 30]);
    let overflow = vec![("otel.metric.overflow".to_owned(), "true".to_owned())];
    assert_eq!(observed.get(&overflow), Some(&30));
    assert_eq!(observed.get(&Vec::new()), Some(&11));
    provider.force_flush().expect("the provider flushes again");
    assert_eq!(
        points(&exporter),
        observed,
        "collections read absolute values anew"
    );
    let warnings = drain
        .queued_records()
        .into_iter()
        .filter(|record| {
            record.message() == "observable instrument recorded series past its bound into overflow"
        })
        .count();
    assert_eq!(
        warnings, 1,
        "one instrument records its first overflow once"
    );
    drop(first);
    provider
        .force_flush()
        .expect("the provider observes the remaining owner");
    let mut remaining: Vec<_> = points(&exporter).values().copied().collect();
    remaining.sort_unstable();
    assert_eq!(remaining, [3, 17]);
    drop(second);
    exporter.reset();
    provider
        .force_flush()
        .expect("the provider observes no owners");
    assert!(points(&exporter).is_empty());
    drop(recorder);
    provider.shutdown().expect("the provider stops");
}

#[test]
fn test_observation_sums_saturate_and_zero_labels_share_one_series() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<crate::metrics::Observation<'static, 2>>();
    let mut values = super::ObservationValues::<0>::new(1);
    values.record([], u64::MAX);
    values.record([], 1);
    assert_eq!(values.series.len(), 1);
    assert_eq!(values.series.get(&[]), Some(&(i64::MAX as u64)));
    assert!(!values.has_overflow());
    let mut attributed = super::ObservationValues::<1>::new(1);
    attributed.record(["parse"], i64::MAX as u64);
    attributed.record(["parse"], 1);
    attributed.record(["ready"], u64::MAX);
    attributed.record(["stopped"], 1);
    assert_eq!(attributed.series.len(), 1);
    assert_eq!(attributed.series.get(&["parse"]), Some(&(i64::MAX as u64)));
    assert_eq!(attributed.overflow, Some(i64::MAX as u64));
}
