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
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::{self, Sender, error::TrySendError};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::{LookupSpan, SpanRef};

use crate::drain::{LogDrain, LogSettlement, QueuedRecord};
use crate::record::{LOG_FIELDS_BYTES_MAX, LOG_LABEL_BYTES_MAX, LogRecord, bounded};
use crate::snapshot::{METRIC_TARGET, SNAPSHOT_VALUES_FIELD};

/// Records the queue holds before a send drops one. The queue exists to absorb a burst
/// while the drain writes; a workspace that emits more than this between two flushes is
/// emitting faster than any store could keep.
pub const LOG_QUEUE_RECORDS: usize = 4_096;
/// Bytes of a panic payload the recorded event keeps, at most.
pub const PANIC_PAYLOAD_BYTES_MAX: usize = 4 << 10;
/// Bytes of one span's field members a record keeps, at most: the members of the close
/// record's own fields, and the members of the `fields` object an event record carries for
/// the span. A member is kept whole or left out, so the fields stay a JSON object; the
/// count of members left out follows as [`FIELDS_LEFT_OUT_MEMBER`].
const SPAN_FIELDS_BYTES_MAX: usize = 1 << 10;
/// The member of an event record's fields that carries the root span: the outermost span
/// around the event.
const ROOT_SPAN_MEMBER: &str = "root_span";
/// The member of an event record's fields that carries the nearest span: the span the
/// event was emitted in, when it is not the root span.
const NEAREST_SPAN_MEMBER: &str = "nearest_span";
/// The member that counts the field members a span's set left out at
/// [`SPAN_FIELDS_BYTES_MAX`].
const FIELDS_LEFT_OUT_MEMBER: &str = "fields_left_out";
/// Field names the layer writes itself. A span or event field under one of them is not
/// recorded, so a member the layer writes never meets a field of the same name.
const RESERVED_FIELD_NAMES: [&str; 3] = [
    ROOT_SPAN_MEMBER,
    NEAREST_SPAN_MEMBER,
    FIELDS_LEFT_OUT_MEMBER,
];
/// Most bytes of [`FIELDS_LEFT_OUT_MEMBER`] with its leading comma: a `u64` count prints
/// in at most 20 digits.
const FIELDS_LEFT_OUT_BYTES_MAX: usize = ",\"fields_left_out\":\"\"".len() + 20;
/// Most bytes of a JSON string holding a label bounded at [`LOG_LABEL_BYTES_MAX`]: JSON
/// writes a control character as six bytes, `\u0000`, and adds two quotes.
const QUOTED_LABEL_BYTES_MAX: usize = 6 * LOG_LABEL_BYTES_MAX + 2;
/// Most bytes of the object an event record carries for one span:
/// `{"name":…,"fields":{…}}`.
const SPAN_CONTEXT_BYTES_MAX: usize = "{\"name\":,\"fields\":{}}".len()
    + QUOTED_LABEL_BYTES_MAX
    + SPAN_FIELDS_BYTES_MAX
    + FIELDS_LEFT_OUT_BYTES_MAX;
/// Most bytes the span members add to an event record's fields: both objects, their names,
/// and the commas before them.
const EVENT_SPAN_MEMBERS_BYTES_MAX: usize =
    2 * SPAN_CONTEXT_BYTES_MAX + ",\"root_span\":".len() + ",\"nearest_span\":".len();
// Both span objects fit the record's fields bound with room left for the event's own.
const _: () = assert!(EVENT_SPAN_MEMBERS_BYTES_MAX < LOG_FIELDS_BYTES_MAX / 4 * 3);

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
        span_opened::<Self, S>(attributes, id, &context);
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        context: Context<'_, S>,
    ) {
        span_recorded::<Self, S>(id, values, &context);
    }

    fn on_close(&self, id: tracing::span::Id, context: Context<'_, S>) {
        if let Some(record) = closed_record::<Self, S>(&id, &context) {
            self.send(record);
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        self.send(event_record::<Self, S>(event, &context));
    }
}

/// Keeps the labels of the span `id` opened with `attributes`, for the records of the layer
/// `Owner`.
///
/// Each layer that writes records keeps its own [`OwnedLabels`]: a per-layer filter hands a
/// layer only the spans it admitted, so the capture and the stderr lines may each see a
/// span the other does not.
pub(crate) fn span_opened<Owner: 'static, S>(
    attributes: &tracing::span::Attributes<'_>,
    id: &tracing::span::Id,
    context: &Context<'_, S>,
) where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    let Some(span) = context.span(id) else {
        return;
    };
    let mut fields = RecordedFields::default();
    attributes.record(&mut fields);
    let labels = SpanLabels::opened(span.name(), &fields);
    span.extensions_mut()
        .replace(OwnedLabels::<Owner>::new(labels));
}

/// Adds the fields the span `id` recorded after it opened to the labels `Owner` keeps.
pub(crate) fn span_recorded<Owner: 'static, S>(
    id: &tracing::span::Id,
    values: &tracing::span::Record<'_>,
    context: &Context<'_, S>,
) where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    let Some(span) = context.span(id) else {
        return;
    };
    let mut fields = RecordedFields::default();
    values.record(&mut fields);
    let mut extensions = span.extensions_mut();
    if let Some(owned) = extensions.get_mut::<OwnedLabels<Owner>>() {
        owned.labels.extend(&fields.rest);
    }
}

/// The record of the span `id` closing: its name as the message, its own fields, `span`,
/// `elapsed_ms`, and, when it closes inside another span, `root_span` for the outermost
/// span around it. `None` when `Owner` kept no labels for the span.
pub(crate) fn closed_record<Owner: 'static, S>(
    id: &tracing::span::Id,
    context: &Context<'_, S>,
) -> Option<LogRecord>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    let span = context.span(id)?;
    let mut fields = String::from("{");
    let (component, operation) = {
        let extensions = span.extensions();
        let labels = &extensions.get::<OwnedLabels<Owner>>()?.labels;
        let elapsed_ms = labels.opened_at.elapsed().as_millis();
        if labels.fields.write_into(&mut fields) {
            fields.push(',');
        }
        let _ = write!(
            fields,
            "\"span\":\"closed\",\"elapsed_ms\":\"{elapsed_ms}\""
        );
        (labels.component.clone(), labels.operation.clone())
    };
    let root = span
        .scope()
        .skip(1)
        .filter(|ancestor| ancestor.extensions().get::<OwnedLabels<Owner>>().is_some())
        .last();
    push_span_member::<Owner, _>(&mut fields, ROOT_SPAN_MEMBER, root.as_ref());
    fields.push('}');
    Some(LogRecord::new(
        now_ms(),
        span.metadata().level().as_str(),
        span.metadata().target(),
        &component,
        &operation,
        span.name(),
        &fields,
    ))
}

/// The record of `event`: a metric snapshot record for a snapshot event, otherwise a log
/// record carrying the event's fields, then `root_span` and `nearest_span` from the spans
/// around it that `Owner` kept labels for.
pub(crate) fn event_record<Owner: 'static, S>(
    event: &Event<'_>,
    context: &Context<'_, S>,
) -> LogRecord
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    let mut fields = RecordedFields::default();
    event.record(&mut fields);
    if event.metadata().target() == METRIC_TARGET {
        return fields.metric_snapshot(event.metadata().level().as_str());
    }
    let mut members = fields.members();
    let RecordedFields {
        message,
        mut component,
        mut operation,
        rest: _,
    } = fields;
    let mut nearest = None;
    let mut root = None;
    for span in context.event_scope(event).into_iter().flatten() {
        let extensions = span.extensions();
        let Some(owned) = extensions.get::<OwnedLabels<Owner>>() else {
            continue;
        };
        if component.is_empty() {
            component.clone_from(&owned.labels.component);
        }
        if operation.is_empty() {
            operation.clone_from(&owned.labels.operation);
        }
        drop(extensions);
        if nearest.is_none() {
            nearest = Some(span);
        } else {
            root = Some(span);
        }
    }
    if root.is_none() {
        root = nearest.take();
    }
    push_span_member::<Owner, _>(&mut members, ROOT_SPAN_MEMBER, root.as_ref());
    push_span_member::<Owner, _>(&mut members, NEAREST_SPAN_MEMBER, nearest.as_ref());
    LogRecord::new(
        now_ms(),
        event.metadata().level().as_str(),
        event.metadata().target(),
        &component,
        &operation,
        &message,
        &format!("{{{members}}}"),
    )
}

/// Appends the object `span` keeps for the records inside it to `members`, under `member`.
///
/// The object was written when the span opened and when it recorded a field, so a record
/// pays one copy of at most [`SPAN_CONTEXT_BYTES_MAX`] bytes per span member.
fn push_span_member<'a, Owner: 'static, R>(
    members: &mut String,
    member: &str,
    span: Option<&SpanRef<'a, R>>,
) where
    R: LookupSpan<'a>,
{
    let Some(span) = span else {
        return;
    };
    let extensions = span.extensions();
    let Some(owned) = extensions.get::<OwnedLabels<Owner>>() else {
        return;
    };
    if !(members.is_empty() || members.ends_with('{')) {
        members.push(',');
    }
    let _ = write!(members, "\"{member}\":{}", owned.labels.context);
}

/// The [`SpanLabels`] one record layer keeps in a span's extensions, keyed by the layer
/// type `Owner`, so two record layers on one subscriber never share or overwrite them.
struct OwnedLabels<Owner> {
    labels: SpanLabels,
    owner: PhantomData<fn() -> Owner>,
}

impl<Owner> OwnedLabels<Owner> {
    const fn new(labels: SpanLabels) -> Self {
        Self {
            labels,
            owner: PhantomData,
        }
    }
}

/// What one span keeps in its extensions for the records written while it is open: its
/// labels, its fields, the object the events inside it carry, and the moment it opened.
///
/// An event takes `component` and `operation` from the nearest span that names them when
/// it names none itself. A span sets them once and every event inside it is filed under
/// them, which is what makes a component read return a lane's whole story rather than the
/// lines that repeated the label.
///
/// The moment is what lets a closing span record how long it took: a store fed by events
/// alone could say a rebuild happened and never how long it ran - the first question a
/// wedged workspace raises.
///
/// `fields` carries every other field the span recorded, as JSON object members, so the
/// close record says what the span did and not only that it ended. `context` is the
/// object an event record carries for the span, `{"name":…,"fields":{…}}`, its `fields`
/// holding `component`, `operation`, and the span's other fields. Both member sets keep
/// [`SPAN_FIELDS_BYTES_MAX`]. `context` is written when the span opens and again when it
/// records a field, never per event.
#[derive(Debug)]
struct SpanLabels {
    component: String,
    operation: String,
    fields: SpanFields,
    context_fields: SpanFields,
    quoted_name: String,
    context: String,
    opened_at: Instant,
}

impl SpanLabels {
    /// The labels of the span `name` that opened with `fields`, each label cut at
    /// [`LOG_LABEL_BYTES_MAX`] as its record column is.
    fn opened(name: &str, fields: &RecordedFields) -> Self {
        let component = bounded(&fields.component, LOG_LABEL_BYTES_MAX);
        let operation = bounded(&fields.operation, LOG_LABEL_BYTES_MAX);
        let mut context_fields = SpanFields::default();
        for (label, value) in [("component", &component), ("operation", &operation)] {
            if !value.is_empty() {
                context_fields.push(label, value);
            }
        }
        let mut labels = Self {
            component,
            operation,
            fields: SpanFields::default(),
            context_fields,
            quoted_name: quoted(&bounded(name, LOG_LABEL_BYTES_MAX)),
            context: String::new(),
            opened_at: Instant::now(),
        };
        labels.extend(&fields.rest);
        labels
    }

    /// Appends `rest` to both member sets and writes `context` again.
    fn extend(&mut self, rest: &[(String, String)]) {
        if rest.is_empty() && !self.context.is_empty() {
            return;
        }
        for (name, value) in rest {
            self.fields.push(name, value);
            self.context_fields.push(name, value);
        }
        self.context.clear();
        let _ = write!(
            self.context,
            "{{\"name\":{},\"fields\":{{",
            self.quoted_name
        );
        self.context_fields.write_into(&mut self.context);
        self.context.push_str("}}");
    }
}

/// One span's field members as JSON object text, without the enclosing braces, at most
/// [`SPAN_FIELDS_BYTES_MAX`] bytes, with the count of the members left out at that bound.
#[derive(Debug, Default)]
struct SpanFields {
    members: String,
    left_out: u64,
}

impl SpanFields {
    /// Appends the member `name`: `value` when it fits whole, and counts it left out when
    /// it does not.
    fn push(&mut self, name: &str, value: &str) {
        let member = format!("{}:{}", quoted(name), quoted(value));
        let separator = usize::from(!self.members.is_empty());
        if self.members.len() + separator + member.len() > SPAN_FIELDS_BYTES_MAX {
            self.left_out = self.left_out.saturating_add(1);
            return;
        }
        if separator == 1 {
            self.members.push(',');
        }
        self.members.push_str(&member);
    }

    /// Writes the members into `out`, then [`FIELDS_LEFT_OUT_MEMBER`] when a member was
    /// left out. Returns whether it wrote anything.
    fn write_into(&self, out: &mut String) -> bool {
        out.push_str(&self.members);
        if self.left_out > 0 {
            if !self.members.is_empty() {
                out.push(',');
            }
            let _ = write!(out, "\"{FIELDS_LEFT_OUT_MEMBER}\":\"{}\"", self.left_out);
        }
        !self.members.is_empty() || self.left_out > 0
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

    /// The metric snapshot record of a snapshot event: its `values` field is already the
    /// JSON object the record's fields hold, written within the record's fields bound.
    fn metric_snapshot(&self, level: &str) -> LogRecord {
        let values = self
            .rest
            .iter()
            .find(|(name, _)| name == SNAPSHOT_VALUES_FIELD)
            .map_or("{}", |(_, values)| values.as_str());
        LogRecord::new(
            now_ms(),
            level,
            METRIC_TARGET,
            &self.component,
            &self.operation,
            &self.message,
            values,
        )
        .into_metric()
    }

    /// The remaining fields as a JSON object, always well formed.
    #[cfg(test)]
    fn rendered(&self) -> String {
        format!("{{{}}}", self.members())
    }

    /// Files one recorded field under the member it belongs to.
    fn record(&mut self, field: &Field, value: String) {
        match field.name() {
            "message" => self.message = value,
            "component" => self.component = value,
            "operation" => self.operation = value,
            name if RESERVED_FIELD_NAMES.contains(&name) => {}
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
