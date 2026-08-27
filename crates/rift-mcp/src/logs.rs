//! Recording the server's own diagnostics into the workspace database.
//!
//! Stderr is where a `tracing` event goes by default, and the agent holding the
//! MCP connection cannot read it: the server's terminal belongs to whoever
//! started it. A request that refuses because the index will not settle
//! therefore carries no way to find out why. The layer here copies every event
//! the process filter admits into a bounded queue, and one drain task writes
//! that queue into the workspace database, where `rift://logs` reads it back.
//!
//! The queue is bounded and the send never blocks: a traced call site pays a
//! `try_send`, and a full queue drops the record and counts it. Losing a record
//! is the correct failure here, because the alternative is a log write pausing
//! the code being logged.

use std::fmt::{self, Write as _};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rift_index::{LOG_BATCH_RECORDS_MAX, LogRecord, LogStore};
use rift_protocol::configuration::LogsConfiguration;
use tokio::sync::mpsc::{self, Receiver, Sender, error::TrySendError};
use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Records the queue holds before a send drops one. The queue exists to absorb
/// a burst while the drain task writes; a workspace that emits more than this
/// between two flushes is emitting faster than any store could keep.
pub const LOG_QUEUE_RECORDS: usize = 4_096;
/// Wall-clock span the drain task waits for more records before writing what it
/// holds.
pub const LOG_FLUSH_INTERVAL: Duration = Duration::from_millis(250);
/// Attempts one batch gets before the drain gives it up. A batch that fails
/// every attempt is dropped and counted, never retried forever: the queue
/// behind it keeps filling while this one waits.
const LOG_WRITE_ATTEMPTS_MAX: u32 = 5;
/// Wall-clock span between two attempts at the same batch.
const LOG_WRITE_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// The `tracing` layer that copies admitted events into the queue.
///
/// Cloning shares one queue: the layer is installed once, and a clone held for
/// a test observes the same drops.
#[derive(Clone, Debug)]
pub struct LogSink {
    sender: Sender<LogRecord>,
    dropped: Arc<AtomicU64>,
}

impl LogSink {
    /// How many records the queue has dropped for being full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Queues one record, counting a drop rather than waiting for room.
    fn send(&self, record: LogRecord) {
        match self.sender.try_send(record) {
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Ok(()) | Err(TrySendError::Closed(_)) => {}
        }
    }
}

/// The queue's reading end, and the task that writes it into the store.
#[derive(Debug)]
pub struct LogDrain {
    receiver: Receiver<LogRecord>,
    dropped: Arc<AtomicU64>,
}

impl LogDrain {
    /// Writes records until the queue closes, draining buffered records after cancellation.
    ///
    /// Records are written in batches: the task takes what the queue holds,
    /// waits [`LOG_FLUSH_INTERVAL`] for more, and writes at most
    /// [`LOG_BATCH_RECORDS_MAX`] per call. Each write trims the store back to
    /// `retention_records`.
    ///
    /// A write that fails is reported once to stderr and its batch is dropped:
    /// the alternative is a retry loop that logs its own failures into the queue
    /// it is failing to drain.
    ///
    /// # Cancel safety
    ///
    /// Cancellation closes the receiving end and drains the bounded queue before return.
    pub async fn run(
        mut self,
        store: Arc<LogStore>,
        retention_records: u64,
        cancellation: CancellationToken,
    ) {
        let mut batch = Vec::with_capacity(LOG_BATCH_RECORDS_MAX);
        let mut closing = false;
        loop {
            let received = tokio::select! {
                biased;
                () = cancellation.cancelled(), if !closing => {
                    self.receiver.close();
                    closing = true;
                    continue;
                }
                received = self.receiver.recv_many(&mut batch, LOG_BATCH_RECORDS_MAX) => received,
            };
            if received == 0 {
                break;
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled(), if !closing => {
                    self.receiver.close();
                    closing = true;
                }
                () = tokio::time::sleep(LOG_FLUSH_INTERVAL), if !closing => {}
                else => {}
            }
            while batch.len() < LOG_BATCH_RECORDS_MAX {
                match self.receiver.try_recv() {
                    Ok(record) => batch.push(record),
                    Err(_) => break,
                }
            }
            self.note_drops(&mut batch);
            self.write_batch(&store, &batch, retention_records).await;
            batch.clear();
        }
        self.note_drops(&mut batch);
        if !batch.is_empty() {
            self.write_batch(&store, &batch, retention_records).await;
        }
    }

    /// Writes one batch, retrying a refused write before giving it up.
    ///
    /// In-process writers queue before reaching `SQLite`. A refusal can still come from
    /// another process or an operating failure. A batch that still fails after
    /// [`LOG_WRITE_ATTEMPTS_MAX`] is counted as dropped, which the next batch
    /// records: the queue behind this one is still filling.
    async fn write_batch(&self, store: &LogStore, batch: &[LogRecord], retention_records: u64) {
        for attempt in 1..=LOG_WRITE_ATTEMPTS_MAX {
            match store.append(batch, retention_records).await {
                Ok(_dropped) => return,
                Err(error) if attempt == LOG_WRITE_ATTEMPTS_MAX => {
                    self.dropped
                        .fetch_add(batch.len() as u64, Ordering::Relaxed);
                    eprintln!(
                        "rift: the log store refused a batch of {} after {attempt} attempts: \
                         {error}{cause}",
                        batch.len(),
                        cause = caused_by(&error)
                    );
                }
                Err(_) => tokio::time::sleep(LOG_WRITE_RETRY_INTERVAL).await,
            }
        }
    }

    /// Appends one record naming the drops so far, when there are any. The
    /// count is what a reader needs to know the run is missing records.
    fn note_drops(&self, batch: &mut Vec<LogRecord>) {
        if batch.len() == LOG_BATCH_RECORDS_MAX {
            return;
        }
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped == 0 {
            return;
        }
        batch.push(LogRecord::new(
            now_ms(),
            "warn",
            "rift_mcp::logs",
            "logs",
            "logs.drain",
            "the log queue was full and dropped records",
            &format!("{{\"dropped\":{dropped}}}"),
        ));
    }
}

/// Builds the layer and its drain, sharing one bounded queue.
#[must_use]
pub fn log_capture() -> (LogSink, LogDrain) {
    let (sender, receiver) = mpsc::channel(LOG_QUEUE_RECORDS);
    let dropped = Arc::new(AtomicU64::new(0));
    (
        LogSink {
            sender,
            dropped: Arc::clone(&dropped),
        },
        LogDrain { receiver, dropped },
    )
}

/// The workspace's `[logs]` table, or the default table while `rift.toml` is
/// absent or invalid.
///
/// The process reads this before it installs tracing, so the capture filter is
/// in force for the startup diagnostics too: a server that refuses to start is
/// one whose records a reader needs most.
#[must_use]
pub fn logs_configuration(root: &Path) -> LogsConfiguration {
    crate::validation::ConfigurationState::accept(root).logs_configuration()
}

impl<S> Layer<S> for LogSink
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        context: Context<'_, S>,
    ) {
        let Some(span) = context.span(id) else {
            return;
        };
        let mut fields = RecordedFields::default();
        attributes.record(&mut fields);
        span.extensions_mut().insert(SpanLabels {
            component: fields.component,
            operation: fields.operation,
            opened_at: Instant::now(),
        });
    }

    fn on_close(&self, id: tracing::span::Id, context: Context<'_, S>) {
        let Some(span) = context.span(&id) else {
            return;
        };
        let extensions = span.extensions();
        let Some(labels) = extensions.get::<SpanLabels>() else {
            return;
        };
        let elapsed_ms = labels.opened_at.elapsed().as_millis();
        self.send(LogRecord::new(
            now_ms(),
            span.metadata().level().as_str(),
            span.metadata().target(),
            &labels.component,
            &labels.operation,
            span.name(),
            &format!("{{\"span\":\"closed\",\"elapsed_ms\":\"{elapsed_ms}\"}}"),
        ));
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let mut fields = RecordedFields::default();
        event.record(&mut fields);
        let (component, operation) = labels(&fields, &context, event);
        self.send(LogRecord::new(
            now_ms(),
            event.metadata().level().as_str(),
            event.metadata().target(),
            &component,
            &operation,
            &fields.message,
            &fields.rendered(),
        ));
    }
}

/// The `component` and `operation` an event carries, falling back to the
/// nearest enclosing span that names them. A span sets them once and every
/// event inside it is filed under them, which is what makes a component read
/// return a lane's whole story rather than the lines that repeated the label.
fn labels<S>(
    fields: &RecordedFields,
    context: &Context<'_, S>,
    event: &Event<'_>,
) -> (String, String)
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    let mut component = fields.component.clone();
    let mut operation = fields.operation.clone();
    if !component.is_empty() && !operation.is_empty() {
        return (component, operation);
    }
    let Some(scope) = context.event_scope(event) else {
        return (component, operation);
    };
    for span in scope {
        let extensions = span.extensions();
        let Some(labels) = extensions.get::<SpanLabels>() else {
            continue;
        };
        if component.is_empty() {
            component.clone_from(&labels.component);
        }
        if operation.is_empty() {
            operation.clone_from(&labels.operation);
        }
        if !component.is_empty() && !operation.is_empty() {
            break;
        }
    }
    (component, operation)
}

/// The labels one span carries, kept in its extensions for the events inside
/// it, with the moment the span opened.
///
/// The moment is what lets a closing span record how long it took. Stderr gets
/// that from the fmt layer's own close line, which no other layer ever sees, so
/// a store fed by events alone could say a rebuild happened and never how long
/// it ran - the first question a wedged workspace raises.
#[derive(Debug)]
struct SpanLabels {
    component: String,
    operation: String,
    opened_at: Instant,
}

/// The fields one event or span recorded: its message, the two labels the
/// codebase files diagnostics under, and everything else as JSON.
#[derive(Debug, Default)]
struct RecordedFields {
    message: String,
    component: String,
    operation: String,
    rest: Vec<(String, String)>,
}

impl RecordedFields {
    /// The remaining fields as a JSON object, always well formed.
    fn rendered(&self) -> String {
        let mut rendered = String::from("{");
        for (index, (name, value)) in self.rest.iter().enumerate() {
            if index > 0 {
                rendered.push(',');
            }
            let _ = write!(rendered, "{}:{}", quoted(name), quoted(value));
        }
        rendered.push('}');
        rendered
    }

    /// Files one recorded field under the member it belongs to.
    fn record(&mut self, field: &Field, value: String) {
        match field.name() {
            "message" => self.message = value,
            "component" => self.component = value,
            "operation" => self.operation = value,
            name => self.rest.push((name.to_owned(), value)),
        }
    }
}

impl Visit for RecordedFields {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_owned());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record(field, value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record(field, value.to_string());
    }
}

/// One JSON string, with the characters JSON reserves escaped.
fn quoted(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            control if control < ' ' => {
                let _ = write!(quoted, "\\u{:04x}", control as u32);
            }
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// One failure's causes, in order, as text a reader can act on. The wrapped
/// label alone names the layer that refused; the chain names what the database
/// actually said.
fn caused_by(error: &(dyn std::error::Error + 'static)) -> String {
    let mut rendered = String::new();
    let mut previous = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if text != previous {
            let _ = write!(rendered, ": {text}");
            previous = text;
        }
        source = cause.source();
    }
    rendered
}

/// Milliseconds since the Unix epoch, or zero on a clock before it.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use rift_index::{
        DatabasePool, LOG_BATCH_RECORDS_MAX, LogQuery, LogRecord, LogStore, WorkspaceDatabase,
    };
    use tokio_util::sync::CancellationToken;

    use super::{LOG_QUEUE_RECORDS, RecordedFields, log_capture, quoted};
    use tracing::field::Visit;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    /// Drains what the queue currently holds, without a store.
    fn queued(drain: &mut super::LogDrain) -> Vec<rift_index::LogRecord> {
        let mut records = Vec::new();
        while let Ok(record) = drain.receiver.try_recv() {
            records.push(record);
        }
        records
    }

    fn record(message: &str) -> LogRecord {
        LogRecord::new(
            1,
            "info",
            "rift_mcp::logs",
            "logs",
            "logs.test",
            message,
            "{}",
        )
    }

    async fn store() -> (tempfile::TempDir, Arc<LogStore>) {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let database =
            WorkspaceDatabase::open(&directory.path().join("db"), DatabasePool::new(4, 1_000))
                .await
                .expect("the database opens");
        (directory, Arc::new(LogStore::attached(database)))
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

    /// The records a case cares about: the events, without the span-close
    /// records the layer writes when a span ends.
    fn events(records: Vec<rift_index::LogRecord>) -> Vec<rift_index::LogRecord> {
        records
            .into_iter()
            .filter(|record| !record.fields().contains("\"span\":\"closed\""))
            .collect()
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

        let records = queued(&mut drain);
        let closed = records
            .iter()
            .find(|record| record.fields().contains("\"span\":\"closed\""))
            .expect("a closed span is recorded");
        assert_eq!(closed.message(), "index.build");
        assert_eq!(closed.component(), "index");
        assert_eq!(closed.operation(), "index.rebuild");
        assert!(
            closed.fields().contains("elapsed_ms"),
            "{}",
            closed.fields()
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

    #[tokio::test]
    async fn cancellation_drains_every_buffered_record() {
        let (_directory, store) = store().await;
        let (sink, drain) = log_capture();
        for message in ["one", "two", "three"] {
            sink.send(record(message));
        }
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        drain.run(Arc::clone(&store), 10_000, cancellation).await;

        assert_eq!(store.count().await.expect("the count reads"), 3);
    }

    #[tokio::test]
    async fn a_full_batch_defers_the_drop_record_without_oversizing_the_write() {
        let (_directory, store) = store().await;
        let (sink, drain) = log_capture();
        for index in 0..(LOG_QUEUE_RECORDS + 8) {
            sink.send(record(&format!("record {index}")));
        }
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        drain.run(Arc::clone(&store), 10_000, cancellation).await;

        assert_eq!(
            store.count().await.expect("the count reads"),
            (LOG_QUEUE_RECORDS + 1) as u64
        );
        let latest = store
            .recent(&LogQuery::newest(1))
            .await
            .expect("the latest record reads");
        assert_eq!(
            latest[0].record().message(),
            "the log queue was full and dropped records"
        );
        assert_eq!(latest[0].record().fields(), "{\"dropped\":8}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_persistently_refused_batch_is_counted_as_dropped() {
        let (_directory, store) = store().await;
        let (_sink, drain) = log_capture();
        let oversized = vec![record("oversized"); LOG_BATCH_RECORDS_MAX + 1];

        drain.write_batch(&store, &oversized, 10_000).await;

        assert_eq!(
            drain.dropped.load(Ordering::Relaxed),
            oversized.len() as u64
        );
        assert_eq!(store.count().await.expect("the count reads"), 0);
    }

    #[test]
    fn a_closed_drain_does_not_report_queue_pressure() {
        let (sink, drain) = log_capture();
        drop(drain);

        sink.send(record("after shutdown"));

        assert_eq!(sink.dropped(), 0);
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
            "rift_mcp::logs",
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
}
