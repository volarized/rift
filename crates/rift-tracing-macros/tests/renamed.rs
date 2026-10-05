//! `#[timed]` in a crate that depends on `rift-tracing` under another name, `tracer`, and on
//! no backend library: every expansion here resolves through the renamed dependency.

use tracer::{ScopedRecorder, SeriesValue, timed};

#[timed("fixture.add")]
fn add(left: u8, right: u8) -> u8 {
    left + right
}

#[timed("fixture.parse", component = "fixture", digits = text.len())]
fn parse(text: &str) -> Result<u8, std::num::ParseIntError> {
    let parsed = text.parse::<u8>()?;
    Ok(parsed)
}

struct Store {
    name: String,
}

impl Store {
    #[timed("fixture.name", component = "fixture")]
    fn name(&self) -> &str {
        &self.name
    }

    #[timed("fixture.load")]
    async fn load<Value: Clone>(&self, value: &Value) -> (Value, &str) {
        (value.clone(), &self.name)
    }
}

fn calls(recorder: &ScopedRecorder, operation: &str) -> Option<SeriesValue> {
    recorder
        .metrics()
        .find(
            "traces.span.metrics.calls",
            &[("span.name", operation), ("status.code", "Ok")],
        )
        .map(|series| series.value().clone())
}

#[test]
fn every_attributed_function_runs_once_and_records_its_operation() {
    let (recorder, mut drain) = ScopedRecorder::builder()
        .install()
        .expect("the default filter parses");
    assert_eq!(add(2, 3), 5);
    assert_eq!(parse("42"), Ok(42));
    assert!(parse("forty-two").is_err(), "`?` leaves the function");
    let store = Store {
        name: "index".to_owned(),
    };
    assert_eq!(store.name(), "index");

    assert_eq!(calls(&recorder, "fixture.add"), Some(SeriesValue::Sum(1.0)));
    assert_eq!(
        calls(&recorder, "fixture.parse"),
        Some(SeriesValue::Sum(2.0))
    );
    assert_eq!(
        calls(&recorder, "fixture.name"),
        Some(SeriesValue::Sum(1.0))
    );
    drop(recorder);
    let closes: Vec<_> = drain
        .queued_records()
        .into_iter()
        .filter(|record| record.message() == "fixture.parse")
        .collect();
    assert_eq!(closes.len(), 2, "one span close per call");
    assert_eq!(closes[0].component(), "fixture");
    assert!(
        closes[0].fields().contains("\"digits\":\"2\""),
        "{}",
        closes[0].fields()
    );
    assert!(
        !closes[0].fields().contains("\"text\""),
        "an argument is recorded only as an explicit field"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_async_method_returns_its_borrowed_value_through_the_await() {
    let (recorder, _drain) = ScopedRecorder::builder()
        .install()
        .expect("the default filter parses");
    let store = Store {
        name: "vectors".to_owned(),
    };
    let (value, name) = store.load(&7_u16).await;
    assert_eq!((value, name), (7, "vectors"));
    assert_eq!(
        calls(&recorder, "fixture.load"),
        Some(SeriesValue::Sum(1.0))
    );
}

#[timed("fixture.first_even")]
fn first_even<Item: Copy + Into<u64>>(items: &[Item]) -> Option<Item> {
    for item in items {
        if (*item).into() % 2 == 0 {
            return Some(*item);
        }
    }
    None
}

#[timed("fixture.refuse")]
fn refuse() -> u8 {
    panic!("the work panics")
}

#[test]
fn an_early_return_finishes_and_a_panic_records_an_error() {
    let (recorder, _drain) = ScopedRecorder::builder()
        .install()
        .expect("the default filter parses");
    assert_eq!(first_even(&[3_u8, 4, 6]), Some(4));
    assert_eq!(first_even::<u32>(&[1, 3]), None);
    assert!(std::panic::catch_unwind(refuse).is_err());

    assert_eq!(
        calls(&recorder, "fixture.first_even"),
        Some(SeriesValue::Sum(2.0))
    );
    let panicked = recorder.metrics();
    let panicked = panicked.find(
        "traces.span.metrics.calls",
        &[
            ("span.name", "fixture.refuse"),
            ("status.code", "Error"),
            ("error.type", "panic"),
        ],
    );
    assert_eq!(
        panicked.map(|series| series.value().clone()),
        Some(SeriesValue::Sum(1.0))
    );
}
