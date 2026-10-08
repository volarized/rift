//! Re-exported tracing macros preserve control flow and registered error metrics.

use tracer::{ScopedRecorder, SeriesValue};

#[tokio::test(flavor = "current_thread")]
async fn facade_only_consumer_keeps_control_flow_and_registered_error_metrics() {
    let (recorder, mut drain) = ScopedRecorder::builder().install().expect("recorder");
    assert_eq!(consumer::expression(), 3);
    let mut calls = 0;
    assert_eq!(consumer::block(&mut calls), 7);
    assert_eq!(calls, 1);
    assert_eq!(consumer::question(false).expect("question success"), 9);
    let refused = consumer::question(true).expect_err("question failure");
    assert_eq!(consumer::early_return(false).expect("return success"), 11);
    assert!(consumer::early_return(true).is_err());
    tracer::traced!("fixture.outer", {
        assert_eq!(consumer::parent(&tracer::Span::current()), 13);
    });
    assert_eq!(consumer::loop_exits(), [1, 3, 5]);
    assert_eq!(
        consumer::asynchronous(false).await.expect("async success"),
        17
    );
    assert!(consumer::asynchronous(true).await.is_err());

    let metrics = recorder.metrics();
    for operation in ["fixture.question", "fixture.return", "fixture.async"] {
        let failed = metrics
            .find(
                "traces.span.metrics.calls",
                &[
                    ("span.name", operation),
                    ("span.kind", "Internal"),
                    ("status.code", "Error"),
                    ("error.type", refused.slug().as_str()),
                ],
            )
            .expect("registered error metric");
        assert_eq!(failed.value(), &SeriesValue::Sum(1.0));
    }
    drop(recorder);
    let records = drain.queued_records();
    let parent = records
        .iter()
        .find(|record| record.message() == "fixture.parent")
        .expect("parent operation close");
    assert_eq!(parent.component(), "fixture");
    assert!(parent.fields().contains("\"units\":\"3\""));
}
