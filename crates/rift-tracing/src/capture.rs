//! The `tracing` layer that copies admitted spans and events into the log drain's queue.
//!
//! Stderr is where a `tracing` event goes by default, and the agent holding the MCP
//! connection cannot read it: the server's terminal belongs to whoever started it. The
//! layer here copies every event the process filter admits into a bounded queue, and the
//! log drain writes that queue into the metrics database, where `rift://logs` reads it
//! back.
//!
//! The queue is bounded and the send never blocks: a traced call site pays a `try_send`,
//! and a full queue drops the record and counts it. Losing a record is the correct
//! failure here, because the alternative is a log write pausing the code being logged.

use std::fmt::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::{self, Sender, error::TrySendError};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::drain::{LogDrain, LogSettlement, QueuedRecord};
use crate::record::{LogRecord, bounded};

/// Records the queue holds before a send drops one. The queue exists to absorb a burst
/// while the drain writes; a workspace that emits more than this between two flushes is
/// emitting faster than any store could keep.
pub const LOG_QUEUE_RECORDS: usize = 4_096;
/// Bytes of a panic payload the recorded event keeps, at most.
pub const PANIC_PAYLOAD_BYTES_MAX: usize = 4 << 10;
/// Bytes of one span's own fields the close record keeps, at most. A longer set is cut
/// at a character boundary, the way a message past
/// [`LOG_MESSAGE_BYTES_MAX`](crate::LOG_MESSAGE_BYTES_MAX) is, and the bound leaves the
/// close record's `span` and `elapsed_ms` members room under
/// [`LOG_FIELDS_BYTES_MAX`](crate::LOG_FIELDS_BYTES_MAX).
const SPAN_FIELDS_BYTES_MAX: usize = 1 << 10;

/// The `tracing` layer that copies admitted events into the queue.
///
/// Cloning shares one queue: the layer is installed once, and a clone held for a test
/// observes the same drops.
#[derive(Clone, Debug)]
pub struct LogSink {
    sender: Sender<QueuedRecord>,
    dropped: Arc<AtomicU64>,
    pub(crate) settlement: Arc<LogSettlement>,
    /// The copies a scoped recorder prints when its test panics.
    #[cfg(any(test, feature = "fixtures"))]
    retained: Option<Arc<crate::recorder::RetainedRecords>>,
}

impl LogSink {
    /// How many records the queue has dropped for being full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Queues one record, counting a drop rather than waiting for room.
    ///
    /// The record takes its sequence before the send, and a send that finds no room
    /// finishes it again: a read waiting on the sequence must never wait for a record no
    /// drain sees.
    pub(crate) fn send(&self, record: LogRecord) {
        #[cfg(any(test, feature = "fixtures"))]
        if let Some(retained) = &self.retained {
            retained.keep(&record);
        }
        let sequence = self.settlement.accept();
        match self.sender.try_send(QueuedRecord { sequence, record }) {
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.settlement.finish_dropped();
            }
            Err(TrySendError::Closed(_)) => self.settlement.finish_dropped(),
            Ok(()) => {}
        }
    }

    /// Keeps a copy of every record this sink sends in `retained`.
    #[cfg(any(test, feature = "fixtures"))]
    pub(crate) fn retaining(mut self, retained: Arc<crate::recorder::RetainedRecords>) -> Self {
        self.retained = Some(retained);
        self
    }
}

/// Builds the layer and its drain, sharing one bounded queue and one settlement.
///
/// A `rift://logs` read finds the settlement through the dispatcher the layer is
/// installed in.
#[must_use]
pub fn log_capture() -> (LogSink, LogDrain) {
    let (sender, receiver) = mpsc::channel(LOG_QUEUE_RECORDS);
    let dropped = Arc::new(AtomicU64::new(0));
    let settlement = Arc::new(LogSettlement::default());
    (
        LogSink {
            sender,
            dropped: Arc::clone(&dropped),
            settlement: Arc::clone(&settlement),
            #[cfg(any(test, feature = "fixtures"))]
            retained: None,
        },
        LogDrain::new(receiver, dropped, settlement),
    )
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
        let members = bounded(&fields.members(), SPAN_FIELDS_BYTES_MAX);
        span.extensions_mut().insert(SpanLabels {
            component: fields.component,
            operation: fields.operation,
            fields: members,
            opened_at: Instant::now(),
        });
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        context: Context<'_, S>,
    ) {
        let Some(span) = context.span(id) else {
            return;
        };
        let mut fields = RecordedFields::default();
        values.record(&mut fields);
        let members = fields.members();
        let mut extensions = span.extensions_mut();
        if let Some(labels) = extensions.get_mut::<SpanLabels>() {
            labels.extend(&members);
        }
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
        let mut fields = String::from("{");
        if !labels.fields.is_empty() {
            fields.push_str(&labels.fields);
            fields.push(',');
        }
        let _ = write!(
            fields,
            "\"span\":\"closed\",\"elapsed_ms\":\"{elapsed_ms}\"}}"
        );
        self.send(LogRecord::new(
            now_ms(),
            span.metadata().level().as_str(),
            span.metadata().target(),
            &labels.component,
            &labels.operation,
            span.name(),
            &fields,
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

/// The `component` and `operation` an event carries, falling back to the nearest
/// enclosing span that names them. A span sets them once and every event inside it is
/// filed under them, which is what makes a component read return a lane's whole story
/// rather than the lines that repeated the label.
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

/// The labels one span carries, kept in its extensions for the events inside it, with
/// its remaining fields and the moment the span opened.
///
/// The moment is what lets a closing span record how long it took. Stderr gets that from
/// the fmt layer's own close line, which no other layer ever sees, so a store fed by
/// events alone could say a rebuild happened and never how long it ran - the first
/// question a wedged workspace raises.
///
/// `fields` carries every other field the span recorded, as JSON object members, so the
/// close record says what the span did and not only that it ended. It is cut at
/// [`SPAN_FIELDS_BYTES_MAX`] on a character boundary.
#[derive(Debug)]
struct SpanLabels {
    component: String,
    operation: String,
    fields: String,
    opened_at: Instant,
}

impl SpanLabels {
    /// Appends `members` to the fields the close record carries, cut back to
    /// [`SPAN_FIELDS_BYTES_MAX`] at a character boundary.
    fn extend(&mut self, members: &str) {
        if members.is_empty() {
            return;
        }
        if !self.fields.is_empty() {
            self.fields.push(',');
        }
        self.fields.push_str(members);
        self.fields = bounded(&self.fields, SPAN_FIELDS_BYTES_MAX);
    }
}

/// The fields one event or span recorded: its message, the two labels the codebase
/// files diagnostics under, and everything else as JSON.
#[derive(Debug, Default)]
struct RecordedFields {
    message: String,
    component: String,
    operation: String,
    rest: Vec<(String, String)>,
}

impl RecordedFields {
    /// The remaining fields as JSON object members, without the enclosing braces, in the
    /// order they were recorded.
    fn members(&self) -> String {
        let mut members = String::new();
        for (index, (name, value)) in self.rest.iter().enumerate() {
            if index > 0 {
                members.push(',');
            }
            let _ = write!(members, "{}:{}", quoted(name), quoted(value));
        }
        members
    }

    /// The remaining fields as a JSON object, always well formed.
    fn rendered(&self) -> String {
        format!("{{{}}}", self.members())
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

/// One JSON string, escaped by `serde_json`.
fn quoted(value: &str) -> String {
    serde_json::Value::from(value).to_string()
}

/// Installs the panic hook that records a panic before the default hook prints it.
///
/// A detached server's panic reaches nobody otherwise: its standard error is a file at
/// best, and the default hook writes there alone. The installed hook emits one `ERROR`
/// event carrying the payload and the source location, through whatever subscriber the
/// panicking thread runs under, then hands the panic to the hook that was installed
/// before it. Installing twice chains the hooks, so a second call records each panic
/// twice; the server installs it once, before it serves.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = panic_payload(info.payload());
        let location = info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_default();
        tracing::error!(
            component = "mcp",
            operation = "server.panic",
            payload,
            location,
            "the server panicked"
        );
        previous(info);
    }));
}

/// The panic payload as text, cut at [`PANIC_PAYLOAD_BYTES_MAX`] on a character boundary.
///
/// `panic!` with a literal carries a `&str`; a formatted message carries a `String`; any
/// other payload is named by its absence.
fn panic_payload(payload: &(dyn std::any::Any + Send)) -> String {
    let text = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<payload is not text>".to_owned());
    bounded(&text, PANIC_PAYLOAD_BYTES_MAX)
}

/// Milliseconds since the Unix epoch, or zero on a clock before it.
pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests;
