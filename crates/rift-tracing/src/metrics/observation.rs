//! Per-collection sums for the observable counter's bounded label sets.

use std::collections::BTreeMap;

use opentelemetry::{KeyValue, metrics::AsyncInstrument};

/// The SDK's label for measurements past an instrument's cardinality limit.
const OVERFLOW_ATTRIBUTE: &str = "otel.metric.overflow";

pub(super) struct ObservationValues<const LABELS: usize> {
    series: BTreeMap<[&'static str; LABELS], u64>,
    cardinality_limit: usize,
    overflow: Option<u64>,
}

impl<const LABELS: usize> ObservationValues<LABELS> {
    pub(super) fn new(cardinality_limit: usize) -> Self {
        Self {
            series: BTreeMap::new(),
            cardinality_limit,
            overflow: None,
        }
    }

    pub(super) fn record(&mut self, labels: [&'static str; LABELS], value: u64) {
        if let Some(held) = self.series.get_mut(&labels) {
            *held = sum(*held, value);
            return;
        }
        let without_labels = labels.iter().all(|label| label.is_empty());
        let attributed_count =
            self.series.len() - usize::from(self.series.contains_key(&[""; LABELS]));
        if without_labels || attributed_count < self.cardinality_limit {
            self.series.insert(labels, sum(0, value));
        } else {
            self.overflow = Some(sum(self.overflow.unwrap_or(0), value));
        }
    }

    pub(super) fn has_overflow(&self) -> bool {
        self.overflow.is_some()
    }

    pub(super) fn publish(
        self,
        instrument: &dyn AsyncInstrument<i64>,
        label_keys: &'static [&'static str; LABELS],
    ) {
        for (labels, value) in self.series {
            let (attributes, count) = super::attributes(label_keys, labels);
            instrument.observe(
                i64::try_from(value).unwrap_or(i64::MAX),
                &attributes[..count],
            );
        }
        if let Some(value) = self.overflow {
            instrument.observe(
                i64::try_from(value).unwrap_or(i64::MAX),
                &[KeyValue::new(OVERFLOW_ATTRIBUTE, true)],
            );
        }
    }
}

fn sum(left: u64, right: u64) -> u64 {
    left.saturating_add(right).min(i64::MAX as u64)
}

#[cfg(test)]
mod tests;
