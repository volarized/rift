use tracing::field::Visit as _;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use super::{
    EVENT_SPAN_MEMBERS_BYTES_MAX, LOG_QUEUE_RECORDS, PANIC_PAYLOAD_BYTES_MAX, RecordedFields,
    SPAN_FIELDS_BYTES_MAX, install_panic_hook, log_capture, panic_payload, quoted,
};
use crate::{LOG_LABEL_BYTES_MAX, LogDrain, LogRecord};

/// Drains what the queue currently holds, without a store.
fn queued(drain: &mut LogDrain) -> Vec<LogRecord> {
    std::iter::from_fn(|| drain.try_recv_record().ok()).collect()
}

fn record(message: &str) -> LogRecord {
    LogRecord::new(
        1,
        "info",
        "rift_tracing::capture",
        "logs",
        "logs.test",
        message,
        "{}",
    )
}

/// The records a case cares about: the events, without the span-close records the layer
/// writes when a span ends.
fn events(records: Vec<LogRecord>) -> Vec<LogRecord> {
    records
        .into_iter()
        .filter(|record| !record.fields().contains("\"span\":\"closed\""))
        .collect()
}

/// The one span-close record a case wrote.
fn closed(records: Vec<LogRecord>) -> LogRecord {
    records
        .into_iter()
        .find(|record| record.fields().contains("\"span\":\"closed\""))
        .expect("a closed span is recorded")
}

#[test]
fn an_event_reaches_the_queue_with_its_labels() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(
            component = "index",
            operation = "index.reconcile",
            epoch = 7,
            "the workspace settled"
        );
    });

    let records = queued(&mut drain);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].message(), "the workspace settled");
    assert_eq!(records[0].level(), "info");
    assert_eq!(records[0].component(), "index");
    assert_eq!(records[0].operation(), "index.reconcile");
    assert_eq!(records[0].fields(), "{\"epoch\":\"7\"}");
}

#[test]
fn an_event_inherits_its_span_labels() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "index.reconcile",
            component = "index",
            operation = "fingerprint.capture"
        );
        let _entered = span.enter();
        tracing::warn!("the capture disagreed with the publication");
    });

    let records = events(queued(&mut drain));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].component(), "index");
    assert_eq!(records[0].operation(), "fingerprint.capture");
}

#[test]
fn a_closing_span_records_how_long_it_ran() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "index.build",
            component = "index",
            operation = "index.rebuild"
        );
        span.in_scope(|| {});
    });

    let closed = closed(queued(&mut drain));
    assert_eq!(closed.message(), "index.build");
    assert_eq!(closed.component(), "index");
    assert_eq!(closed.operation(), "index.rebuild");
    assert!(
        closed.fields().contains("elapsed_ms"),
        "{}",
        closed.fields()
    );
}

/// A span records what it opened with and what it recorded later, so a reader of
/// `rift://logs` sees the fields standard error shows. A field declared through the
/// facade's [`empty!`](crate::empty) placeholder records nothing until it is given a
/// value.
#[test]
fn a_closing_span_records_the_fields_it_carried() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        let span = crate::info_span!(
            "index.build",
            component = "index",
            operation = "index.rebuild",
            trigger = "filesystem",
            epoch = 7,
            changed_count = crate::empty!(),
            never_recorded = crate::empty!(),
        );
        span.record("changed_count", 3);
        span.in_scope(|| {});
    });

    let closed = closed(queued(&mut drain));
    assert!(
        closed.fields().starts_with(
            "{\"code.function.name\":\"rift_tracing::capture::tests::\
             a_closing_span_records_the_fields_it_carried\",\"trigger\":\"filesystem\",\
             \"epoch\":\"7\",\"changed_count\":\"3\",\
             \"span\":\"closed\","
        ),
        "{}",
        closed.fields()
    );
    assert!(
        closed.fields().contains("\"elapsed_ms\":"),
        "{}",
        closed.fields()
    );
}

/// A span carrying nothing past its two labels records how long it ran alone.
#[test]
fn a_span_without_fields_records_how_long_it_ran_alone() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "index.build",
            component = "index",
            operation = "index.rebuild"
        );
        span.in_scope(|| {});
    });

    let closed = closed(queued(&mut drain));
    assert!(
        closed
            .fields()
            .starts_with("{\"span\":\"closed\",\"elapsed_ms\":\""),
        "{}",
        closed.fields()
    );
}

/// A span closing inside another carries the outermost span as `root_span` after its own
/// members, so its close names the request it ran for; the outermost span's own close
/// carries no span member.
#[test]
fn a_span_closing_inside_another_carries_the_root_span() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        let request = tracing::info_span!("mcp.request", component = "mcp", request_id = 7);
        let _request = request.enter();
        let middle = tracing::info_span!("index.reconcile", attempts = 2);
        let _middle = middle.enter();
        tracing::info_span!("fingerprint.fold", operation = "fingerprint.fold").in_scope(|| {});
    });

    let closes = queued(&mut drain)
        .into_iter()
        .map(|record| {
            let fields: serde_json::Value =
                serde_json::from_str(record.fields()).expect("a close record's fields are JSON");
            (record.message().to_owned(), fields)
        })
        .collect::<Vec<_>>();
    let names = closes
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["fingerprint.fold", "index.reconcile", "mcp.request"]
    );
    let root = serde_json::json!({
        "name": "mcp.request",
        "fields": {"component": "mcp", "request_id": "7"},
    });
    assert_eq!(closes[0].1["root_span"], root);
    assert_eq!(closes[1].1["root_span"], root);
    assert_eq!(closes[1].1["attempts"], "2");
    assert!(closes[2].1.get("root_span").is_none(), "{:?}", closes[2].1);
    assert!(
        closes[0].1.get("nearest_span").is_none(),
        "{:?}",
        closes[0].1
    );
}

/// A span whose fields run past [`SPAN_FIELDS_BYTES_MAX`] keeps the members that fit whole
/// and counts the ones left out, so its close record stays a JSON object.
#[test]
fn a_span_past_the_field_bound_records_the_members_that_fit() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);
    let long = "é".repeat(SPAN_FIELDS_BYTES_MAX);

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "index.build",
            component = "index",
            operation = "index.rebuild",
            epoch = 7,
            detail = long.as_str(),
            trigger = "filesystem"
        );
        span.in_scope(|| {});
    });

    let closed = closed(queued(&mut drain));
    let fields = closed.fields();
    assert!(
        fields.starts_with(
            "{\"epoch\":\"7\",\"trigger\":\"filesystem\",\"fields_left_out\":\"1\",\
             \"span\":\"closed\","
        ),
        "{fields}"
    );
    let object: serde_json::Value =
        serde_json::from_str(fields).expect("the close record's fields are a JSON object");
    assert!(object.get("detail").is_none(), "{fields}");
}

/// The records a case wrote inside `subscriber`, as JSON objects of their fields keyed by
/// message, the span close records left out.
fn event_fields(records: Vec<LogRecord>) -> Vec<(String, serde_json::Value)> {
    events(records)
        .into_iter()
        .map(|record| {
            let fields = serde_json::from_str(record.fields())
                .unwrap_or_else(|error| panic!("{error}: {}", record.fields()));
            (record.message().to_owned(), fields)
        })
        .collect()
}

/// An event inside one span carries the span's name and fields under `root_span`, after its
/// own fields, and no `nearest_span`: the root span is the nearest one.
#[test]
fn an_event_inside_a_span_carries_the_span_fields() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "mcp.request",
            component = "mcp",
            operation = "tools/call",
            request_id = 7,
            tool = "search"
        );
        span.in_scope(|| tracing::info!(is_error = false, "tool request completed"));
    });

    let records = events(queued(&mut drain));
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].fields(),
        "{\"is_error\":\"false\",\"root_span\":{\"name\":\"mcp.request\",\"fields\":\
         {\"component\":\"mcp\",\"operation\":\"tools/call\",\"request_id\":\"7\",\"tool\":\"search\"}}}"
    );
    assert_eq!(records[0].component(), "mcp");
    assert_eq!(records[0].operation(), "tools/call");
}

/// Inside nested spans an event carries the outermost span as `root_span` and the span it
/// was emitted in as `nearest_span`; a span between the two is in neither.
#[test]
fn an_event_inside_nested_spans_carries_the_root_and_the_nearest_span() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        let root = tracing::info_span!("mcp.request", component = "mcp", request_id = 7);
        let _root = root.enter();
        let middle = tracing::info_span!("index.reconcile", attempts = 2);
        let _middle = middle.enter();
        tracing::info!("between");
        let nearest = tracing::debug_span!("worker.queue", component = "worker", work = "search");
        nearest.in_scope(|| tracing::info!("inside"));
    });

    let records = event_fields(queued(&mut drain));
    let between = &records[0].1;
    assert_eq!(records[0].0, "between");
    assert_eq!(between["root_span"]["name"], "mcp.request");
    assert_eq!(between["nearest_span"]["name"], "index.reconcile");
    assert_eq!(
        between["nearest_span"]["fields"],
        serde_json::json!({"attempts": "2"})
    );
    let inside = &records[1].1;
    assert_eq!(records[1].0, "inside");
    assert_eq!(
        inside,
        &serde_json::json!({
            "root_span": {
                "name": "mcp.request",
                "fields": {"component": "mcp", "request_id": "7"},
            },
            "nearest_span": {
                "name": "worker.queue",
                "fields": {"component": "worker", "work": "search"},
            },
        })
    );
}

/// The function that opens the span of [`a_field_recorded_after_open_reaches_later_events`].
const RECORDED_AFTER_OPEN: &str =
    "rift_tracing::capture::tests::a_field_recorded_after_open_reaches_later_events";

/// A field the span records after it opened is on the events that follow the record and
/// not on the ones before it.
#[test]
fn a_field_recorded_after_open_reaches_later_events() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        let span = crate::info_span!(
            "mcp.forward",
            component = "mcp",
            upstream_request_id = crate::empty!(),
        );
        span.in_scope(|| tracing::info!("before"));
        span.record("upstream_request_id", 12);
        span.in_scope(|| tracing::info!("after"));
    });

    let records = event_fields(queued(&mut drain));
    assert_eq!(records[0].0, "before");
    assert_eq!(
        records[0].1["root_span"]["fields"],
        serde_json::json!({"component": "mcp", "code.function.name": RECORDED_AFTER_OPEN})
    );
    assert_eq!(records[1].0, "after");
    assert_eq!(
        records[1].1["root_span"]["fields"],
        serde_json::json!({
            "component": "mcp",
            "code.function.name": RECORDED_AFTER_OPEN,
            "upstream_request_id": "12",
        })
    );
}

/// A span past [`SPAN_FIELDS_BYTES_MAX`] reaches its events with the members that fit and
/// the count left out, and the event's fields stay a JSON object within
/// [`EVENT_SPAN_MEMBERS_BYTES_MAX`] of span members.
#[test]
fn an_event_inside_a_span_past_the_field_bound_carries_the_members_that_fit() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);
    let long = "é".repeat(SPAN_FIELDS_BYTES_MAX);
    let wide = "é".repeat(400);

    tracing::subscriber::with_default(subscriber, || {
        let root = tracing::info_span!(
            "mcp.request",
            component = long.as_str(),
            operation = "tools/call",
            detail = long.as_str(),
            request_id = 7
        );
        let _root = root.enter();
        let nearest =
            tracing::info_span!("index.build", first = wide.as_str(), second = wide.as_str());
        nearest.in_scope(|| tracing::info!(own = 1, "inside"));
    });

    let records = events(queued(&mut drain));
    let fields = records[0].fields();
    assert!(
        fields.len() <= EVENT_SPAN_MEMBERS_BYTES_MAX + "{\"own\":\"1\",}".len(),
        "{}",
        fields.len()
    );
    let object: serde_json::Value = serde_json::from_str(fields).expect("a JSON object");
    let root = &object["root_span"]["fields"];
    assert_eq!(root["request_id"], "7", "{fields}");
    assert!(root.get("detail").is_none(), "{fields}");
    assert!(
        root["component"]
            .as_str()
            .is_some_and(|component| component.len() == LOG_LABEL_BYTES_MAX),
        "the label is cut where its record column is: {fields}"
    );
    assert_eq!(root["fields_left_out"], "1", "{fields}");
    let nearest = &object["nearest_span"]["fields"];
    assert!(nearest.get("first").is_some(), "{fields}");
    assert!(nearest.get("second").is_none(), "{fields}");
    assert_eq!(nearest["fields_left_out"], "1", "{fields}");
    assert_eq!(object["own"], "1");
}

/// The span members of an event are written by the layer alone: a field the code names
/// `root_span`, `nearest_span`, or `fields_left_out` is not recorded, inside a span or
/// outside one.
#[test]
fn a_field_under_a_reserved_name_is_not_recorded() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(root_span = "forged", epoch = 1, "outside");
        let span = tracing::info_span!("mcp.request", request_id = 7, fields_left_out = 9);
        span.in_scope(|| tracing::info!(root_span = "forged", nearest_span = "forged", "inside"));
    });

    let records = events(queued(&mut drain));
    assert_eq!(records[0].fields(), "{\"epoch\":\"1\"}");
    assert_eq!(
        records[1].fields(),
        "{\"root_span\":{\"name\":\"mcp.request\",\"fields\":{\"request_id\":\"7\"}}}"
    );
}

/// An event outside every span carries its own fields alone.
#[test]
fn an_event_outside_every_span_carries_its_own_fields_alone() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!("mcp.request", request_id = 7);
        drop(span.enter());
        tracing::info!(epoch = 7, "outside");
        tracing::info!("bare");
    });

    let records = events(queued(&mut drain));
    assert_eq!(records[0].fields(), "{\"epoch\":\"7\"}");
    assert_eq!(records[1].fields(), "{}");
}

/// A task spawned from inside an instrumented future runs outside its span, so its events
/// carry no span member; a task under `.instrument(span)` carries that span alone, here a
/// span opened with no parent. The current-thread runtime polls a spawned task only once
/// the instrumented future has returned `Pending` and left its span.
#[test]
fn a_spawned_task_carries_only_the_span_it_was_instrumented_with() {
    use tracing::Instrument as _;

    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds");

    tracing::subscriber::with_default(subscriber, || {
        runtime.block_on(
            async {
                tracing::info!("request");
                tokio::spawn(async { tracing::info!("detached") })
                    .await
                    .expect("the detached task completes");
                let lane =
                    tracing::info_span!(parent: None, "validation.lane", component = "validation");
                tokio::spawn(async { tracing::info!("instrumented") }.instrument(lane))
                    .await
                    .expect("the instrumented task completes");
            }
            .instrument(tracing::info_span!("mcp.request", request_id = 7)),
        );
    });

    let records = event_fields(queued(&mut drain));
    let messages: Vec<&str> = records
        .iter()
        .map(|(message, _)| message.as_str())
        .collect();
    assert_eq!(messages, ["request", "detached", "instrumented"]);
    assert_eq!(records[0].1["root_span"]["fields"]["request_id"], "7");
    assert_eq!(records[1].1, serde_json::json!({}));
    assert_eq!(records[2].1["root_span"]["name"], "validation.lane");
    assert!(
        records[2].1["root_span"]["fields"]
            .get("request_id")
            .is_none()
    );
}

#[test]
fn a_full_queue_drops_and_counts() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink.clone());

    tracing::subscriber::with_default(subscriber, || {
        for index in 0..(LOG_QUEUE_RECORDS + 8) {
            tracing::info!(index, "filling the queue");
        }
    });

    assert_eq!(sink.dropped(), 8);
    assert_eq!(queued(&mut drain).len(), LOG_QUEUE_RECORDS);
}

#[test]
fn a_closed_drain_does_not_report_queue_pressure() {
    let (sink, drain) = log_capture();
    drop(drain);

    sink.send(record("after shutdown"));

    assert_eq!(sink.dropped(), 0);
}

/// The hook is process-global: the thread panics under its own subscriber, so the event
/// the hook emits lands in this case's queue and nowhere else.
#[test]
fn a_panic_under_the_hook_is_recorded_with_its_payload_and_location() {
    install_panic_hook();
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    let joined = std::thread::spawn(move || {
        tracing::subscriber::with_default(subscriber, || {
            panic!("injected panic for the hook");
        });
    })
    .join();

    assert!(joined.is_err(), "the thread must have panicked");
    let records = events(queued(&mut drain));
    let recorded = records
        .iter()
        .find(|record| record.message() == "the server panicked")
        .expect("the hook records the panic");
    assert_eq!(recorded.level(), "error");
    assert_eq!(recorded.component(), "mcp");
    assert_eq!(recorded.operation(), "server.panic");
    assert!(
        recorded.fields().contains("injected panic for the hook"),
        "{}",
        recorded.fields()
    );
    // The fields are JSON text, which escapes each `\` of a Windows path; the decoded
    // location carries the compiler's spelling of this file, the one `file!()` names.
    let fields: serde_json::Value =
        serde_json::from_str(recorded.fields()).expect("the fields are a JSON object");
    let location = fields["location"]
        .as_str()
        .expect("the record carries the panic location");
    assert!(
        location.starts_with(&format!("{}:", file!())),
        "the location names this file: {}",
        recorded.fields()
    );
}

/// A panic publishes the table of operations in flight, at `WARN`, with the reason
/// `panic`: the operation the panicking thread ran is still open when the hook runs.
#[test]
fn a_panic_under_the_hook_publishes_the_operations_in_flight()
-> Result<(), Box<dyn std::error::Error>> {
    install_panic_hook();
    let (recorder, mut drain) = crate::ScopedRecorder::builder().install()?;

    let caught = std::panic::catch_unwind(|| {
        crate::traced!(component = "mcp", operation = "server.stop", {
            panic!("injected panic inside an operation");
        });
    });
    drop(recorder);

    assert!(caught.is_err(), "the operation must have panicked");
    let records = drain.queued_records();
    let published = records
        .iter()
        .find(|record| record.message() == "operations in flight")
        .ok_or("the hook publishes the table")?;
    assert_eq!(published.level(), "warn");
    assert_eq!(published.target(), "rift_tracing::flight");
    let fields: serde_json::Value = serde_json::from_str(published.fields())?;
    assert_eq!(fields["reason"], "panic");
    assert_eq!(fields["in_flight"], "1");
    let operations: serde_json::Value = serde_json::from_str(
        fields["operations"]
            .as_str()
            .ok_or("the table lists its entries as text")?,
    )?;
    assert_eq!(operations[0]["operation"], "server.stop");
    Ok(())
}

#[test]
fn a_panic_payload_is_text_cut_at_its_bound() {
    let literal: Box<dyn std::any::Any + Send> = Box::new("literal");
    assert_eq!(panic_payload(literal.as_ref()), "literal");

    let formatted: Box<dyn std::any::Any + Send> = Box::new(String::from("formatted"));
    assert_eq!(panic_payload(formatted.as_ref()), "formatted");

    let opaque: Box<dyn std::any::Any + Send> = Box::new(7_u8);
    assert_eq!(panic_payload(opaque.as_ref()), "<payload is not text>");

    let oversized: Box<dyn std::any::Any + Send> = Box::new("é".repeat(PANIC_PAYLOAD_BYTES_MAX));
    let cut = panic_payload(oversized.as_ref());
    assert!(cut.len() <= PANIC_PAYLOAD_BYTES_MAX);
    assert!(cut.chars().all(|character| character == 'é'));
}

#[test]
fn a_panic_payload_cut_moves_back_to_a_character_boundary() {
    let three_byte_characters: Box<dyn std::any::Any + Send> =
        Box::new("€".repeat(PANIC_PAYLOAD_BYTES_MAX));
    let cut = panic_payload(three_byte_characters.as_ref());
    assert_eq!(
        cut.len(),
        PANIC_PAYLOAD_BYTES_MAX - PANIC_PAYLOAD_BYTES_MAX % 3
    );
    assert!(cut.chars().all(|character| character == '€'));
}

#[test]
fn a_json_string_escapes_what_json_reserves() {
    assert_eq!(
        quoted("a\"b\\c\n\r\t\u{0001}d"),
        "\"a\\\"b\\\\c\\n\\r\\t\\u0001d\""
    );
}

#[test]
fn rendered_fields_are_a_json_object() {
    let mut fields = RecordedFields::default();
    fields.record_bool(&field("first"), true);

    assert_eq!(fields.rendered(), "{\"first\":\"true\"}");
}

#[test]
fn errors_are_recorded_by_their_display_text() {
    let mut fields = RecordedFields::default();
    fields.record_error(&field("first"), &std::io::Error::other("disk refused"));

    assert_eq!(fields.rendered(), "{\"first\":\"disk refused\"}");
}

/// One field of a callsite this suite can record against.
fn field(name: &'static str) -> tracing::field::Field {
    struct Callsite;
    impl tracing::Callsite for Callsite {
        fn set_interest(&self, _interest: tracing::subscriber::Interest) {}
        fn metadata(&self) -> &tracing::Metadata<'_> {
            &METADATA
        }
    }
    static CALLSITE: Callsite = Callsite;
    static METADATA: tracing::Metadata<'static> = tracing::Metadata::new(
        "fields",
        "rift_tracing::capture",
        tracing::Level::INFO,
        None,
        None,
        None,
        tracing::field::FieldSet::new(&["first"], tracing::callsite::Identifier(&CALLSITE)),
        tracing::metadata::Kind::EVENT,
    );
    METADATA
        .fields()
        .field(name)
        .unwrap_or_else(|| unreachable!("the callsite declares {name}"))
}

#[test]
fn the_initialized_layer_records_through_the_global_subscriber() {
    let (sink, mut drain) = log_capture();
    let guard = crate::capture::registry().with(sink).set_default();

    tracing::error!(component = "logs", "a global record");
    drop(guard);

    let records = queued(&mut drain);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].level(), "error");
}

/// The fields of the one record whose message is `message`, as JSON.
fn fields_of(records: &[LogRecord], message: &str) -> serde_json::Value {
    let record = records
        .iter()
        .find(|record| record.message() == message)
        .unwrap_or_else(|| panic!("a record says {message}: {records:?}"));
    serde_json::from_str(record.fields()).expect("a record's fields are a JSON object")
}

/// A type whose methods open operations, for the function name a record carries.
struct Workspace {
    component: &'static str,
}

/// A trait whose method opens an operation.
trait Rebuild {
    fn rebuild(&self);
}

impl Workspace {
    fn search(&self) {
        crate::traced!(component = self.component, operation = "index.search", {});
    }

    async fn nodes(&self) {
        crate::traced!("index.nodes", async {}).await;
    }

    fn walk(&self) {
        let visit = || crate::info!(component = self.component, "walked");
        visit();
    }
}

impl Rebuild for Workspace {
    fn rebuild(&self) {
        crate::traced!("index.rebuild", {});
    }
}

/// A free function that opens an operation through `#[timed]`.
#[crate::timed("index.watch")]
fn watch() {}

/// Every operation and event records the function that opened or emitted it as
/// `code.function.name`, without the `::{{closure}}` segments of an async body or a
/// closure.
#[test]
fn an_operation_and_an_event_record_the_function_that_made_them() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        let workspace = Workspace { component: "index" };
        workspace.search();
        workspace.rebuild();
        workspace.walk();
        watch();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime builds");
        runtime.block_on(workspace.nodes());
    });

    let records = queued(&mut drain);
    let module = "rift_tracing::capture::tests";
    for (message, function) in [
        ("index.search", format!("{module}::Workspace::search")),
        (
            "index.rebuild",
            format!("<{module}::Workspace as {module}::Rebuild>::rebuild"),
        ),
        ("walked", format!("{module}::Workspace::walk")),
        ("index.watch", format!("{module}::watch")),
        ("index.nodes", format!("{module}::Workspace::nodes")),
    ] {
        assert_eq!(
            fields_of(&records, message)["code.function.name"],
            function.as_str(),
            "{message}"
        );
    }
}

/// A close record states how long its operation was entered and how long it waited,
/// in nanoseconds: an awaited operation suspended on a timer is idle for the wait.
#[tokio::test(start_paused = false)]
async fn a_close_record_carries_busy_and_idle_time_across_a_suspension() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);
    let _default = tracing::subscriber::set_default(subscriber);

    crate::traced!("index.wait", async {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    })
    .await;

    let fields = fields_of(&queued(&mut drain), "index.wait");
    let nanoseconds = |member: &str| {
        fields[member]
            .as_str()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("{member} is a count: {fields}"))
    };
    let (busy, idle) = (nanoseconds("busy_ns"), nanoseconds("idle_ns"));
    assert!(idle >= 20_000_000, "the timer wait is idle: {fields}");
    assert!(busy < idle, "polling is shorter than the wait: {fields}");
    assert_eq!(fields["status.code"], "Ok", "{fields}");
    assert!(fields.get("error.type").is_none(), "{fields}");
}

/// A close record states that its operation did not complete: `panic` for a block left
/// by a panic, `cancelled` for an awaited operation dropped after its first poll.
#[test]
fn a_close_record_states_a_panic_and_a_cancellation() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        let panicked = std::panic::catch_unwind(|| {
            crate::traced!("index.panics", {
                std::panic::panic_any("refused");
            });
        });
        assert!(panicked.is_err());
        let mut pending = Box::pin(crate::traced!("index.cancelled", async {
            std::future::pending::<()>().await;
        }));
        let waker = std::task::Waker::noop();
        let polled = pending
            .as_mut()
            .poll(&mut std::task::Context::from_waker(waker));
        assert!(polled.is_pending());
        drop(pending);
        crate::traced!("index.completes", {});
    });

    let records = queued(&mut drain);
    for (message, status, error) in [
        ("index.panics", "Error", Some("panic")),
        ("index.cancelled", "Error", Some("cancelled")),
        ("index.completes", "Ok", None),
    ] {
        let fields = fields_of(&records, message);
        assert_eq!(fields["status.code"], status, "{fields}");
        assert_eq!(
            fields.get("error.type").and_then(|v| v.as_str()),
            error,
            "{fields}"
        );
    }
}

/// Runs one operation per way of ending, each recording how it ended on its own span.
fn record_failures_on_spans() {
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    let outcome = |name: &'static str, value: &'static str| {
        let span = crate::Span::current();
        span.record("outcome", value);
        name
    };
    crate::traced!(
        component = "test",
        operation = "test.outcome_error",
        outcome = crate::empty!(),
        { outcome("test.outcome_error", "error") }
    );
    crate::traced!(
        component = "test",
        operation = "test.outcome_timeout",
        outcome = crate::empty!(),
        { outcome("test.outcome_timeout", "timeout") }
    );
    crate::traced!(
        component = "test",
        operation = "test.error_type",
        outcome = crate::empty!(),
        {
            outcome("test.error_type", "timeout");
            crate::Span::current().record("error.type", "index.lexical_storage");
        }
    );
    crate::traced!(
        component = "test",
        operation = "test.outcome_ok",
        outcome = crate::empty!(),
        { outcome("test.outcome_ok", "ok") }
    );
    let mut awaited = pin!(crate::traced!(
        component = "test",
        operation = "test.awaited_error",
        outcome = crate::empty!(),
        async { outcome("test.awaited_error", "error") }
    ));
    assert_eq!(
        awaited
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready("test.awaited_error")
    );
    let build = tracing::info_span!(
        "index.build",
        component = "index",
        outcome = tracing::field::Empty
    );
    build.in_scope(|| build.record("outcome", "error"));
    drop(build);
    let wait = tracing::info_span!(
        "lock.wait",
        component = "lock",
        outcome = tracing::field::Empty
    );
    wait.record("outcome", "acquired");
    drop(wait);
}

/// A span that records a failure itself ends failed in its close record, its printed
/// close, and the operation metrics alike: `outcome` other than `ok` or `acquired`, or
/// any `error.type`. The record keeps the recorded `error.type`; the metric label is that
/// value when the metrics list it, `_OTHER` otherwise.
#[test]
fn a_failure_the_span_records_ends_the_record_the_line_and_the_metric_failed() {
    let (recorder, mut drain) = crate::ScopedRecorder::builder()
        .capture("trace")
        .install()
        .expect("the capture filter parses");
    record_failures_on_spans();

    let records = queued(&mut drain);
    let snapshot = recorder.metrics();
    for (message, status, error_type, label, close) in [
        (
            "test.outcome_error",
            "Error",
            None,
            Some("_OTHER"),
            "close ✗ busy=",
        ),
        (
            "test.outcome_timeout",
            "Error",
            None,
            Some("timeout"),
            "close ✗ busy=",
        ),
        (
            "test.error_type",
            "Error",
            Some("index.lexical_storage"),
            Some("_OTHER"),
            "close ✗ error.type=index.lexical_storage busy=",
        ),
        ("test.outcome_ok", "Ok", None, None, "close ✓ busy="),
        (
            "test.awaited_error",
            "Error",
            None,
            Some("_OTHER"),
            "close ✗ busy=",
        ),
        ("index.build", "Error", None, None, "close ✗ busy="),
        ("lock.wait", "Ok", None, None, "close ✓ busy="),
    ] {
        let fields = fields_of(&records, message);
        assert_eq!(fields["status.code"], status, "{message}: {fields}");
        assert_eq!(
            fields.get("error.type").and_then(serde_json::Value::as_str),
            error_type,
            "{message}: {fields}"
        );
        let line = records
            .iter()
            .find(|record| record.message() == message)
            .map(LogRecord::rendered)
            .unwrap_or_default();
        assert!(line.contains(close), "{message}: {line}");
        if message.starts_with("test.") {
            let mut labels = vec![("span.name", message), ("status.code", status)];
            labels.extend(label.map(|label| ("error.type", label)));
            assert!(
                snapshot
                    .find("traces.span.metrics.calls", &labels)
                    .is_some(),
                "{message} counts under {labels:?}: {snapshot:?}"
            );
        }
    }
}

/// A span the capture filter leaves out still names the records inside it: the event
/// carries it as its root or nearest span, as the stderr line prints it.
#[test]
fn an_event_inside_a_span_the_capture_leaves_out_carries_that_span() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(crate::runtime::capture_layer(
        sink,
        tracing_subscriber::EnvFilter::new("rift_tracing=info,hidden=off"),
    ));

    tracing::subscriber::with_default(subscriber, || {
        let request = tracing::info_span!(
            "mcp.request",
            component = "mcp",
            operation = "tools/call",
            request_id = 4
        );
        let _request = request.enter();
        let hidden = tracing::info_span!(
            target: "hidden",
            "dependency.context",
            component = "dependency",
            operation = "dependency.context"
        );
        hidden.in_scope(|| crate::info!(entries = 1, "context read"));
    });

    let records = queued(&mut drain);
    assert!(
        records
            .iter()
            .all(|record| record.message() != "dependency.context"),
        "the left-out span writes no close record: {records:?}"
    );
    let event = records
        .iter()
        .find(|record| record.message() == "context read")
        .expect("the event is captured");
    assert_eq!(event.component(), "dependency");
    assert_eq!(event.operation(), "dependency.context");
    let fields: serde_json::Value = serde_json::from_str(event.fields()).expect("JSON");
    assert_eq!(fields["root_span"]["name"], "mcp.request", "{fields}");
    assert_eq!(fields["root_span"]["fields"]["request_id"], "4", "{fields}");
    assert_eq!(
        fields["nearest_span"]["name"], "dependency.context",
        "{fields}"
    );
}

/// A dependency's span without labels around a Rift request span is no root: the request
/// span is, as the line groups records by request.
#[test]
fn a_dependency_span_without_labels_is_not_the_root() {
    let (sink, mut drain) = log_capture();
    let subscriber = crate::capture::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || {
        let serve = tracing::info_span!(target: "rmcp::service", "serve_inner");
        let _serve = serve.enter();
        let request = crate::info_span!("mcp.request", component = "mcp", request_id = 1);
        request.in_scope(|| crate::info!("tool request started"));
    });

    let records = queued(&mut drain);
    let event = fields_of(&records, "tool request started");
    assert_eq!(event["root_span"]["name"], "mcp.request", "{event}");
    assert!(event.get("nearest_span").is_none(), "{event}");
    let close = fields_of(&records, "mcp.request");
    assert!(
        close.get("root_span").is_none(),
        "the request span is a root: {close}"
    );
    let serve = fields_of(&records, "serve_inner");
    assert_eq!(
        serve["status.code"], "Ok",
        "its own close is still recorded: {serve}"
    );
}

/// The cost of one operation's span and of one event inside it, through the layers a
/// serving process installs: the table of operations in flight, the stderr lines under
/// the default stderr filter into `io::sink`, and the capture under the default capture
/// filter into a queue a drain thread empties. 1,000,000 `traced!` block
/// operations per run, split across 1 and 8 threads, then as many events inside one span.
/// Prints the median nanoseconds per operation and per event over five runs of every
/// thread. Run it alone in a release build:
///
/// ```text
/// cargo test -p rift-tracing --release --lib -- --ignored --nocapture operation_record_cost
/// ```
#[test]
#[ignore = "a measurement, not a check; run it alone in a release build"]
fn operation_record_cost() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::Instant;

    use tracing_subscriber::Layer as _;

    use crate::flight::{FlightLayer, FlightTable};
    use crate::render::LevelColor;
    use crate::runtime::{DEFAULT_STDERR_FILTER, DEFAULT_TRACING_FILTER, stderr_filter};
    use crate::stderr::StderrLines;

    const OPERATIONS: u32 = 1_000_000;
    const RUNS: u32 = 5;

    let (sink, mut drain) = log_capture();
    let draining = Arc::new(AtomicBool::new(true));
    let drainer = {
        let draining = Arc::clone(&draining);
        std::thread::spawn(move || {
            while draining.load(Ordering::Relaxed) {
                if drain.try_recv_record().is_err() {
                    std::thread::yield_now();
                }
            }
        })
    };
    let dispatch = tracing::Dispatch::new(
        crate::capture::registry()
            .with(FlightLayer::new(Arc::new(FlightTable::default())))
            .with(
                StderrLines::new(std::io::sink, LevelColor::Plain).with_filter(stderr_filter(
                    tracing_subscriber::EnvFilter::new(DEFAULT_STDERR_FILTER),
                )),
            )
            .with(crate::runtime::capture_layer(
                sink,
                tracing_subscriber::EnvFilter::new(DEFAULT_TRACING_FILTER),
            )),
    );
    let median = |mut samples: Vec<f64>| {
        samples.sort_by(f64::total_cmp);
        samples[samples.len() / 2]
    };
    for threads in [1_u32, 8] {
        let per_thread = OPERATIONS / threads;
        let mut operations = Vec::new();
        let mut events = Vec::new();
        for _ in 0..RUNS {
            let start = Arc::new(Barrier::new(threads as usize));
            let workers: Vec<_> = (0..threads)
                .map(|_| {
                    let dispatch = dispatch.clone();
                    let start = Arc::clone(&start);
                    std::thread::spawn(move || {
                        tracing::dispatcher::with_default(&dispatch, || {
                            start.wait();
                            let opened = Instant::now();
                            for index in 0..per_thread {
                                crate::traced!(
                                    component = "index",
                                    operation = "index.cost",
                                    unit = index,
                                    {}
                                );
                            }
                            let opened = opened.elapsed();
                            let span = tracing::info_span!(
                                "mcp.request",
                                component = "mcp",
                                operation = "tools/call",
                                request_id = 7
                            );
                            let _entered = span.enter();
                            let emitted = Instant::now();
                            for index in 0..per_thread {
                                crate::info!(unit = index, "cost event");
                            }
                            (opened, emitted.elapsed())
                        })
                    })
                })
                .collect();
            for worker in workers {
                let (opened, emitted) = worker.join().expect("a worker finishes");
                operations.push(opened.as_secs_f64() * 1e9 / f64::from(per_thread));
                events.push(emitted.as_secs_f64() * 1e9 / f64::from(per_thread));
            }
        }
        let (operation, event) = (median(operations), median(events));
        println!("threads={threads} operation_ns={operation:.1} event_ns={event:.1}");
    }
    draining.store(false, Ordering::Relaxed);
    drainer.join().expect("the drain thread finishes");
}
