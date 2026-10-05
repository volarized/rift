use tracing::field::Visit as _;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use super::{
    LOG_QUEUE_RECORDS, PANIC_PAYLOAD_BYTES_MAX, RecordedFields, SPAN_FIELDS_BYTES_MAX,
    install_panic_hook, log_capture, panic_payload, quoted,
};
use crate::{LogDrain, LogRecord};

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
    let subscriber = tracing_subscriber::registry().with(sink);

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
    let subscriber = tracing_subscriber::registry().with(sink);

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
    let subscriber = tracing_subscriber::registry().with(sink);

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
    let subscriber = tracing_subscriber::registry().with(sink);

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
            "{\"trigger\":\"filesystem\",\"epoch\":\"7\",\"changed_count\":\"3\",\
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
    let subscriber = tracing_subscriber::registry().with(sink);

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

/// A span whose fields run past [`SPAN_FIELDS_BYTES_MAX`] records the cut form and
/// nothing longer, with the cut on a character boundary.
#[test]
fn a_span_past_the_field_bound_records_the_cut_fields() {
    let (sink, mut drain) = log_capture();
    let subscriber = tracing_subscriber::registry().with(sink);
    let long = "é".repeat(SPAN_FIELDS_BYTES_MAX);

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "index.build",
            component = "index",
            operation = "index.rebuild",
            detail = long.as_str()
        );
        span.in_scope(|| {});
    });

    let closed = closed(queued(&mut drain));
    let fields = closed.fields();
    let closing = fields
        .find(",\"span\":\"closed\"")
        .unwrap_or_else(|| panic!("the close members follow the span's own: {fields}"));
    assert_eq!(closing, 1 + SPAN_FIELDS_BYTES_MAX);
    assert!(fields.starts_with("{\"detail\":\"é"), "{fields}");
    assert!(
        fields[11..closing]
            .chars()
            .all(|character| character == 'é'),
        "{fields}"
    );
}

#[test]
fn a_full_queue_drops_and_counts() {
    let (sink, mut drain) = log_capture();
    let subscriber = tracing_subscriber::registry().with(sink.clone());

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
    let subscriber = tracing_subscriber::registry().with(sink);

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
    let guard = tracing_subscriber::registry().with(sink).set_default();

    tracing::error!(component = "logs", "a global record");
    drop(guard);

    let records = queued(&mut drain);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].level(), "error");
}
