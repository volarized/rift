//! The `tracing` layer that copies admitted spans and events into the log drain's queue.
//!
//! Stderr is where a `tracing` event goes by default, and the agent holding the MCP
//! connection cannot read it: the server's terminal belongs to whoever started it. The
//! layer here copies every event the process filter admits into a bounded queue, and the
//! log drain writes that queue into the metrics database, where `rift server logs` reads it
//! back.
//!
//! The queue is bounded and the send never blocks: a traced call site pays a `try_send`,
//! and a full queue drops the record and counts it. Losing a record is the correct
//! failure here, because the alternative is a log write pausing the code being logged.

use std::cell::RefCell;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use tokio::sync::mpsc::{self, Sender, error::TrySendError};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layered, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

use crate::drain::{LogDeliveryOptions, LogDrain, LogSettlement, QueuedRecord};
use crate::measurement::process_monotonic_now;
use crate::metrics::{Counter, SCOPE};
use crate::record::{LOG_FIELDS_BYTES_MAX, LOG_LABEL_BYTES_MAX, LogRecord, bounded};
use crate::subscriptions::{LOG_SUBSCRIPTION_BYTES_MAX, LogStream, RecordBudget};

/// Records the queue holds before a send drops one. The queue exists to absorb a burst
/// while the drain writes; a workspace that emits more than this between two flushes is
/// emitting faster than any store could keep.
pub const LOG_QUEUE_RECORDS: usize = 4_096;
/// `log.queue.dropped`: records lost before the store, by `error.type`: `queue_full` for a
/// record the full queue refused, `unwritten` for one a stopped drain never wrote.
pub(crate) static LOG_QUEUE_DROPPED: Counter<1> =
    Counter::declare(SCOPE, "log.queue.dropped", "{record}", &["error.type"]);
/// The `error.type` of a record the full queue refused.
pub(crate) const QUEUE_FULL: &str = "queue_full";
/// The `error.type` of a record a drain aborted at its stop deadline never wrote.
pub(crate) const UNWRITTEN: &str = "unwritten";
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
/// The member of a span close record that holds the nanoseconds the span was entered.
const BUSY_MEMBER: &str = "busy_ns";
/// The member of a span close record that holds the nanoseconds the span was open and not
/// entered.
const IDLE_MEMBER: &str = "idle_ns";
/// The member of a span close record that states whether its operation completed: `Ok`, or
/// `Error` beside `error.type`. The spelling of the operation metrics' label.
const STATUS_CODE_MEMBER: &str = "status.code";
/// The member naming why an operation did not complete: `panic`, `cancelled`, or the
/// registered error identity. The spelling of the operation metrics' label.
const ERROR_TYPE_MEMBER: &str = "error.type";
/// The field a span or a lifecycle record states how it ended in.
pub(crate) const OUTCOME_FIELD: &str = "outcome";
/// The `outcome` values that state an operation or a phase completed: `ok`, and
/// `acquired` for a lock wait. Every other value, such as `error`, `timeout`, `refused`, or
/// `cancelled`, states it did not.
const COMPLETED_OUTCOMES: [&str; 2] = ["ok", "acquired"];
/// The `error.type` values the operation metrics keep as their own label beside the
/// registered error identities: a panic, a cancellation, and the lock wait endings. Every
/// other failure records `_OTHER`, the value OpenTelemetry's `error.type` names for a
/// failure no listed value fits; a recorded value can be any string, and a metric label is
/// a declared value.
const ERROR_TYPE_LABELS: [&str; 4] = ["panic", "cancelled", "timeout", "refused"];
/// The `error.type` label of a failure that is neither in [`ERROR_TYPE_LABELS`] nor a
/// registered error identity.
const OTHER_ERROR_TYPE: &str = "_OTHER";
/// Field names the layer writes itself. A span or event field under one of them is not
/// recorded, so a member the layer writes never meets a field of the same name.
const RESERVED_FIELD_NAMES: [&str; 6] = [
    ROOT_SPAN_MEMBER,
    NEAREST_SPAN_MEMBER,
    FIELDS_LEFT_OUT_MEMBER,
    BUSY_MEMBER,
    IDLE_MEMBER,
    STATUS_CODE_MEMBER,
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
/// Cloning shares one stream and its persistence subscription. Each additional
/// subscription has its own bounded queue and loss count.
#[derive(Clone, Debug)]
pub struct LogSink {
    sender: Sender<QueuedRecord>,
    dropped: Arc<AtomicU64>,
    pub(crate) settlement: Arc<LogSettlement>,
    pub(crate) logs: LogStream,
    budget: RecordBudget,
}

impl LogSink {
    /// How many records the queue has dropped for being full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Publishes one record to independent bounded queues without waiting for room.
    ///
    /// Publication orders the sequence and sends together. A send that finds no room
    /// finishes it again: a read waiting on the sequence must never wait for a record no
    /// drain sees. A drop also adds one to `log.queue.dropped` with `error.type`
    /// `queue_full`, through the instrument the meter's install built: the send runs inside
    /// this layer, and building the instrument here would report through `tracing` into it.
    pub(crate) fn send(&self, record: LogRecord) {
        self.logs
            .publish(record, |record| self.send_persistence(record));
    }

    /// Sends to the persistence subscription inside the stream's publication lock.
    fn send_persistence(&self, record: LogRecord) -> u64 {
        let sequence = self.settlement.accept();
        let Some(bytes) = self.budget.reserve(&record) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.settlement.finish_dropped();
            LOG_QUEUE_DROPPED.add_built([QUEUE_FULL], 1);
            return sequence;
        };
        match self.sender.try_send(QueuedRecord {
            sequence,
            record,
            bytes: Some(bytes),
        }) {
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.settlement.finish_dropped();
                LOG_QUEUE_DROPPED.add_built([QUEUE_FULL], 1);
            }
            Err(TrySendError::Closed(_)) => self.settlement.finish_dropped(),
            Ok(()) => {}
        }
        sequence
    }
}

/// Builds the layer and its first subscription, the persistence drain.
///
/// Shutdown finds the settlement through the dispatcher the layer is installed in.
#[must_use]
pub fn log_capture() -> (LogSink, LogDrain) {
    log_capture_with(LogDeliveryOptions::default())
}

/// Builds the persistence subscription under accepted delivery settings.
pub(crate) fn log_capture_with(options: LogDeliveryOptions) -> (LogSink, LogDrain) {
    let (sender, receiver) = mpsc::channel(options.queue_records);
    let dropped = Arc::new(AtomicU64::new(0));
    let settlement = Arc::new(LogSettlement::with_options(options));
    let budget = RecordBudget::new(LOG_SUBSCRIPTION_BYTES_MAX);
    (
        LogSink {
            sender,
            dropped: Arc::clone(&dropped),
            settlement: Arc::clone(&settlement),
            logs: LogStream::new(options.queue_records, true),
            budget: budget.clone(),
        },
        LogDrain::new(receiver, dropped, settlement).with_budget(budget),
    )
}

impl<S> Layer<S> for LogSink
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_close(&self, id: tracing::span::Id, context: Context<'_, S>) {
        if let Some(record) = closed_record(&id, &context) {
            self.send(record);
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        self.send(event_record(event, &context));
    }
}

/// The registry every subscriber that writes records composes on: the bare registry with
/// [`SpanContextLayer`] first, so every span carries what its records need before any
/// record layer runs.
pub(crate) fn registry() -> Layered<SpanContextLayer, Registry> {
    tracing_subscriber::registry().with(SpanContextLayer::default())
}

/// The `tracing` layer that keeps, for every span, what the records written inside it
/// carry: its labels and fields, its root span, and how long it was busy and idle.
///
/// It runs unfiltered and first, before the stderr lines and the capture, which run each
/// under a filter of its own. A per-layer filter hands its layer only the spans it
/// admitted, so a record layer that kept its own labels lost the spans its filter refused:
/// an event inside a span `[logs] capture` left out was stored without that span while
/// stderr printed it. Kept here once, a span's context reaches every record layer whether
/// or not that layer's filter admitted the span, and both layers write one record.
///
/// The busy and idle time follow `tracing-subscriber`'s own close timing
/// (`fmt/fmt_layer.rs`, `on_enter`, `on_exit`, `on_close`): busy is the time the span was
/// entered, idle the time it was not, both from its opening to its close.
///
/// The timings read the process's monotonic clock, or the clock a scoped recorder's builder
/// named: one clock, so the close record and the operation metrics state one elapsed time.
#[derive(Clone, Default)]
pub(crate) struct SpanContextLayer {
    /// The clock [`ScopedRecorderBuilder::clock`](crate::ScopedRecorderBuilder::clock)
    /// named; `None` reads the process's monotonic clock.
    #[cfg(any(test, feature = "fixtures"))]
    clock: Option<Arc<dyn Fn() -> Duration + Send + Sync>>,
}

impl fmt::Debug for SpanContextLayer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpanContextLayer")
            .finish_non_exhaustive()
    }
}

impl SpanContextLayer {
    /// The layer whose timings read `clock` instead of the process's monotonic clock.
    #[cfg(any(test, feature = "fixtures"))]
    pub(crate) fn with_clock(clock: Arc<dyn Fn() -> Duration + Send + Sync>) -> Self {
        Self { clock: Some(clock) }
    }

    /// Reads the layer's clock.
    fn now(&self) -> Duration {
        #[cfg(any(test, feature = "fixtures"))]
        if let Some(clock) = &self.clock {
            return clock();
        }
        process_monotonic_now()
    }
}

/// The reading of the clock a scoped recorder named, when the thread's default subscriber
/// carries one; `None` otherwise, and inside a subscriber callback, where `tracing` hands
/// out no default.
#[cfg(any(test, feature = "fixtures"))]
pub(crate) fn scoped_now() -> Option<Duration> {
    tracing::dispatcher::get_default(|dispatch| {
        dispatch
            .downcast_ref::<SpanContextLayer>()?
            .clock
            .as_ref()
            .map(|clock| clock())
    })
}

impl<S> Layer<S> for SpanContextLayer
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
        let parent = span.parent().and_then(|parent| {
            parent
                .extensions()
                .get::<SpanEntry>()
                .and_then(SpanEntry::context)
        });
        let labeled = span.metadata().target().starts_with(RIFT_TARGET)
            || !(fields.component.is_empty() && fields.operation.is_empty());
        let node = Arc::new(SpanNode::opened(span.name(), &fields, parent.clone()));
        span.extensions_mut().replace(SpanEntry {
            context: if labeled {
                Some(Arc::clone(&node))
            } else {
                parent
            },
            node,
            timings: Timings::opened(self.now()),
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
        if let Some(entry) = span.extensions().get::<SpanEntry>() {
            entry.node.labels_mut().extend(&fields.rest);
        }
    }

    fn on_enter(&self, id: &tracing::span::Id, context: Context<'_, S>) {
        if let Some(span) = context.span(id)
            && let Some(entry) = span.extensions_mut().get_mut::<SpanEntry>()
        {
            entry.timings.entered(self.now());
        }
    }

    fn on_exit(&self, id: &tracing::span::Id, context: Context<'_, S>) {
        if let Some(span) = context.span(id)
            && let Some(entry) = span.extensions_mut().get_mut::<SpanEntry>()
        {
            entry.timings.exited(self.now());
        }
    }

    fn on_close(&self, id: tracing::span::Id, context: Context<'_, S>) {
        if let Some(span) = context.span(&id)
            && let Some(entry) = span.extensions_mut().get_mut::<SpanEntry>()
        {
            entry.timings.closed(self.now());
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let node = context.event_span(event).and_then(|span| {
            span.extensions()
                .get::<SpanEntry>()
                .and_then(SpanEntry::context)
        });
        EVENT_SPAN.with(|held| *held.borrow_mut() = (event_key(event), node));
    }
}

thread_local! {
    /// The span the event this thread is dispatching was emitted in, as
    /// [`SpanContextLayer`] found it, keyed by [`event_key`].
    ///
    /// A record layer under a filter finds an event's spans through its own filter alone,
    /// so it reads the event's span here. The layer runs first for every event, so the
    /// value a record layer reads was written for the event it is handed.
    static EVENT_SPAN: RefCell<(usize, Option<Arc<SpanNode>>)> = const { RefCell::new((0, None)) };
}

/// The identity of `event` while it is dispatched: every layer is handed one reference.
fn event_key(event: &Event<'_>) -> usize {
    std::ptr::from_ref(event).addr()
}

/// The span `event` was emitted in: the one [`SpanContextLayer`] found, or, on a
/// subscriber without that layer, the nearest span `context` admits.
fn event_span<S>(event: &Event<'_>, context: &Context<'_, S>) -> Option<Arc<SpanNode>>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    let key = event_key(event);
    let held = EVENT_SPAN.with(|held| {
        let held = held.borrow();
        (held.0 == key).then(|| held.1.clone())
    });
    held.unwrap_or_else(|| {
        context.event_span(event).and_then(|span| {
            span.extensions()
                .get::<SpanEntry>()
                .and_then(SpanEntry::context)
        })
    })
}

/// The start every Rift crate's target shares.
const RIFT_TARGET: &str = "rift";

/// What [`SpanContextLayer`] keeps in one span's extensions: the span's own node, the
/// node the records inside it name as their span, and its timings.
///
/// A dependency's span that carries neither `component` nor `operation`, such as `rmcp`'s
/// `serve_inner` around Rift's request span, names no context of its own: the records
/// inside it, and the spans that open inside it, see the Rift span around it. Its own
/// close record still names it.
struct SpanEntry {
    node: Arc<SpanNode>,
    context: Option<Arc<SpanNode>>,
    timings: Timings,
}

impl SpanEntry {
    fn context(&self) -> Option<Arc<SpanNode>> {
        self.context.clone()
    }
}

/// How long one span was busy and idle, in nanoseconds, as `tracing-subscriber`'s close
/// timing counts them: busy while entered at least once, idle otherwise. Every reading
/// comes from [`SpanContextLayer`]'s clock.
///
/// `completed` is the elapsed time from the opening to the end of the `traced!` operation
/// the span times, read once when the operation's completion guard drops. The operation
/// metrics and the close record's `elapsed_ms` both take it, so a clone of the span held
/// past the operation lengthens neither.
#[derive(Debug)]
struct Timings {
    busy_ns: u64,
    idle_ns: u64,
    opened: Duration,
    last: Duration,
    entered: u64,
    completed: Option<Duration>,
}

impl Timings {
    const fn opened(now: Duration) -> Self {
        Self {
            busy_ns: 0,
            idle_ns: 0,
            opened: now,
            last: now,
            entered: 0,
            completed: None,
        }
    }

    /// Nanoseconds since the last change, as of `now`.
    fn lap(&mut self, now: Duration) -> u64 {
        let lap = nanoseconds(now.saturating_sub(self.last));
        self.last = now;
        lap
    }

    fn entered(&mut self, now: Duration) {
        if self.entered == 0 {
            self.idle_ns = self.idle_ns.saturating_add(self.lap(now));
        }
        self.entered = self.entered.saturating_add(1);
    }

    fn exited(&mut self, now: Duration) {
        self.entered = self.entered.saturating_sub(1);
        if self.entered == 0 {
            self.busy_ns = self.busy_ns.saturating_add(self.lap(now));
        }
    }

    /// Ends the count at the close: the time since the last exit is idle, or busy for a
    /// span closed while entered.
    fn closed(&mut self, now: Duration) {
        let lap = self.lap(now);
        if self.entered == 0 {
            self.idle_ns = self.idle_ns.saturating_add(lap);
        } else {
            self.busy_ns = self.busy_ns.saturating_add(lap);
        }
    }

    /// Keeps the elapsed time from the opening to `now` as the operation's own, and
    /// returns it. `None`, and nothing kept, when `now` precedes the opening.
    fn completed(&mut self, now: Duration) -> Option<Duration> {
        let elapsed = now.checked_sub(self.opened)?;
        self.completed = Some(elapsed);
        Some(elapsed)
    }

    /// The nanoseconds the close record states as `elapsed_ms`: the operation's own
    /// elapsed time when its completion kept one, else the span's busy and idle time.
    fn elapsed_ns(&self) -> u64 {
        self.completed
            .map_or_else(|| self.busy_ns.saturating_add(self.idle_ns), nanoseconds)
    }
}

/// `duration` in nanoseconds, saturating at `u64::MAX`.
fn nanoseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// One span as the records inside it see it: its labels and fields, the span it opened
/// in, and the outermost span around it.
#[derive(Debug)]
pub(crate) struct SpanNode {
    labels: RwLock<SpanLabels>,
    parent: Option<Arc<Self>>,
    root: Option<Arc<Self>>,
}

impl SpanNode {
    /// The node of the span `name` opened with `fields` inside `parent`.
    fn opened(name: &str, fields: &RecordedFields, parent: Option<Arc<Self>>) -> Self {
        let root = parent
            .as_ref()
            .map(|parent| parent.root.clone().unwrap_or_else(|| Arc::clone(parent)));
        Self {
            labels: RwLock::new(SpanLabels::opened(name, fields)),
            parent,
            root,
        }
    }

    fn labels(&self) -> RwLockReadGuard<'_, SpanLabels> {
        self.labels.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn labels_mut(&self) -> RwLockWriteGuard<'_, SpanLabels> {
        self.labels.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The `component` and `operation` an event inside this span is filed under when it
    /// names none itself: each from the nearest span, this one first, that names it.
    fn inherited(&self, component: &mut String, operation: &mut String) {
        let mut node = Some(self);
        while let Some(current) = node {
            if !component.is_empty() && !operation.is_empty() {
                return;
            }
            let labels = current.labels();
            if component.is_empty() {
                component.clone_from(&labels.component);
            }
            if operation.is_empty() {
                operation.clone_from(&labels.operation);
            }
            drop(labels);
            node = current.parent.as_deref();
        }
    }
}

/// Whether the `outcome` value `value` states its operation or phase completed.
pub(crate) fn completed_outcome(value: &str) -> bool {
    COMPLETED_OUTCOMES.contains(&value)
}

/// The operation metrics' `error.type` label of the failure value `value`: the value
/// itself when [`ERROR_TYPE_LABELS`] lists it or it is a registered error identity
/// (`rift_error::errors::REGISTERED_SLUGS`), [`OTHER_ERROR_TYPE`] otherwise.
fn error_type_label(value: &str) -> &'static str {
    ERROR_TYPE_LABELS
        .into_iter()
        .chain(rift_error::errors::REGISTERED_SLUGS.iter().copied())
        .find(|label| *label == value)
        .unwrap_or(OTHER_ERROR_TYPE)
}

/// The operation metrics' `error.type` label of the failure the open span `id` recorded,
/// as its close record will state it: see [`SpanLabels::failure`]. `None` when the span
/// recorded none, is closed, or the thread's dispatcher keeps no [`SpanContextLayer`]
/// entry for it.
pub(crate) fn span_failure(id: &tracing::span::Id) -> Option<&'static str> {
    tracing::dispatcher::get_default(|dispatch| {
        let span = dispatch.downcast_ref::<Registry>()?.span(id)?;
        let extensions = span.extensions();
        extensions.get::<SpanEntry>()?.node.labels().failure()
    })
}

/// Ends the `traced!` operation the open span `id` times: reads the clock of the thread's
/// [`SpanContextLayer`] once and keeps the elapsed time since the span opened as the
/// operation's own (see [`Timings`]). Returns that elapsed time; `None` when the thread's
/// dispatcher keeps no entry for the span, or the clock reads before the opening.
pub(crate) fn span_completed(id: &tracing::span::Id) -> Option<Duration> {
    tracing::dispatcher::get_default(|dispatch| {
        let now = dispatch.downcast_ref::<SpanContextLayer>()?.now();
        let span = dispatch.downcast_ref::<Registry>()?.span(id)?;
        let mut extensions = span.extensions_mut();
        extensions.get_mut::<SpanEntry>()?.timings.completed(now)
    })
}

/// The record of the span `id` closing: its name as the message, its own fields, then
/// `span`, `elapsed_ms`, `busy_ns`, `idle_ns`, `status.code`, `error.type` when it did not
/// complete, and, when it closes inside another span, `root_span` for the outermost span
/// around it. `None` when the span carries no [`SpanContextLayer`] entry.
///
/// The span completed (`status.code` `Ok`) unless it recorded an `error.type` itself, as
/// `traced!` does for an awaited operation dropped before it returned (`cancelled`), it
/// closes while its thread unwinds a panic (`panic`), or it recorded an `outcome` that is
/// not a completion (see [`completed_outcome`]), as `index.build` records `error`.
pub(crate) fn closed_record<S>(
    id: &tracing::span::Id,
    context: &Context<'_, S>,
) -> Option<LogRecord>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    let span = context.span(id)?;
    let extensions = span.extensions();
    let entry = extensions.get::<SpanEntry>()?;
    let labels = entry.node.labels();
    let Timings {
        busy_ns, idle_ns, ..
    } = entry.timings;
    let elapsed_ms = entry.timings.elapsed_ns() / 1_000_000;
    let mut own = labels.fields.object();
    for (member, value) in [
        ("span", "closed".to_owned()),
        ("elapsed_ms", elapsed_ms.to_string()),
        (BUSY_MEMBER, busy_ns.to_string()),
        (IDLE_MEMBER, idle_ns.to_string()),
    ] {
        own.insert(member.to_owned(), Value::String(value));
    }
    let panicked = labels.error_type.is_none() && std::thread::panicking();
    let completed = labels.error_type.is_none() && !panicked && labels.failed_outcome.is_none();
    let status = if completed { "Ok" } else { "Error" };
    own.insert(STATUS_CODE_MEMBER.to_owned(), Value::from(status));
    if panicked {
        own.insert(ERROR_TYPE_MEMBER.to_owned(), Value::from("panic"));
    }
    let (component, operation) = (labels.component.clone(), labels.operation.clone());
    drop(labels);
    let root = entry.node.root.clone();
    drop(extensions);
    let spans = root
        .as_deref()
        .map(|root| (ROOT_SPAN_MEMBER, root))
        .into_iter()
        .collect::<Vec<_>>();
    Some(LogRecord::new(
        now_ms(),
        span.metadata().level().as_str(),
        span.metadata().target(),
        &component,
        &operation,
        span.name(),
        &record_fields(own, &spans),
    ))
}

/// The record of `event`: a log record carrying the event's fields, then `root_span` and
/// `nearest_span` from the spans around it, whether or not the calling layer's filter
/// admitted them.
pub(crate) fn event_record<S>(event: &Event<'_>, context: &Context<'_, S>) -> LogRecord
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    let mut fields = RecordedFields::default();
    event.record(&mut fields);
    let RecordedFields {
        message,
        mut component,
        mut operation,
        rest,
    } = fields;
    let nearest = event_span(event, context);
    let mut spans = Vec::with_capacity(2);
    if let Some(nearest) = nearest.as_deref() {
        nearest.inherited(&mut component, &mut operation);
        match nearest.root.as_deref() {
            Some(root) => {
                spans.push((ROOT_SPAN_MEMBER, root));
                spans.push((NEAREST_SPAN_MEMBER, nearest));
            }
            None => spans.push((ROOT_SPAN_MEMBER, nearest)),
        }
    }
    let own = rest
        .into_iter()
        .map(|(name, value)| (name, Value::String(value)));
    LogRecord::new(
        now_ms(),
        event.metadata().level().as_str(),
        event.metadata().target(),
        &component,
        &operation,
        &message,
        &record_fields(own, &spans),
    )
}

/// One record's fields as the JSON object `serde_json` writes: the `own` members in the
/// order given, then the object each of `spans` keeps under its member name, at most
/// [`LOG_FIELDS_BYTES_MAX`] bytes.
///
/// The span objects are counted first and always fit (see [`EVENT_SPAN_MEMBERS_BYTES_MAX`]);
/// an own member that does not fit whole in the bytes they leave is left out, and the count
/// of members left out follows as [`FIELDS_LEFT_OUT_MEMBER`]. Each span object was written
/// when the span opened and when it recorded a field, so a record pays one copy of at most
/// [`SPAN_CONTEXT_BYTES_MAX`] bytes per span member.
fn record_fields(
    own: impl IntoIterator<Item = (String, Value)>,
    spans: &[(&'static str, &SpanNode)],
) -> String {
    let maximum = LOG_FIELDS_BYTES_MAX - "{}".len() - FIELDS_LEFT_OUT_BYTES_MAX;
    let contexts = spans
        .iter()
        .map(|(member, span)| {
            let labels = span.labels();
            (*member, labels.context.clone(), labels.context_bytes)
        })
        .collect::<Vec<_>>();
    let reserved = contexts
        .iter()
        .map(|(member, _, bytes)| ",".len() + member_bytes(member, *bytes))
        .sum::<usize>();
    let mut members = BoundedMembers::new(maximum.saturating_sub(reserved));
    for (name, value) in own {
        members.push(name, value);
    }
    members.maximum = maximum;
    for (member, context, bytes) in contexts {
        members.push_measured(member.to_owned(), context, bytes);
    }
    Value::Object(members.into_object()).to_string()
}

/// What one span keeps for the records written while it is open: its labels, its fields,
/// and the object the events inside it carry.
///
/// An event takes `component` and `operation` from the nearest span that names them when
/// it names none itself. A span sets them once and every event inside it is filed under
/// them, which is what makes a component read return a lane's whole story rather than the
/// lines that repeated the label.
///
/// `fields` carries every other field the span recorded, so the close record says what the
/// span did and not only that it ended. `context` is the object an event record carries for
/// the span, `{"name":…,"fields":{…}}`, its `fields` holding `component`, `operation`, and
/// the span's other fields, and `context_bytes` its length as `serde_json` writes it. Both
/// member sets keep [`SPAN_FIELDS_BYTES_MAX`]. `context` is built when the span opens and
/// again when it records a field, never per event. `error_type` holds the operation
/// metrics' label of the `error.type` the span recorded, and `failed_outcome` that of an
/// `outcome` it recorded that is not a completion, so its close states it did not complete.
#[derive(Debug)]
struct SpanLabels {
    component: String,
    operation: String,
    fields: BoundedMembers,
    context_fields: BoundedMembers,
    name: String,
    context: Value,
    context_bytes: usize,
    error_type: Option<&'static str>,
    failed_outcome: Option<&'static str>,
}

impl SpanLabels {
    /// The labels of the span `name` that opened with `fields`, each label cut at
    /// [`LOG_LABEL_BYTES_MAX`] as its record column is.
    fn opened(name: &str, fields: &RecordedFields) -> Self {
        let component = bounded(&fields.component, LOG_LABEL_BYTES_MAX);
        let operation = bounded(&fields.operation, LOG_LABEL_BYTES_MAX);
        let mut context_fields = BoundedMembers::new(SPAN_FIELDS_BYTES_MAX);
        for (label, value) in [("component", &component), ("operation", &operation)] {
            if !value.is_empty() {
                context_fields.push(label.to_owned(), Value::from(value.as_str()));
            }
        }
        let mut labels = Self {
            component,
            operation,
            fields: BoundedMembers::new(SPAN_FIELDS_BYTES_MAX),
            context_fields,
            name: bounded(name, LOG_LABEL_BYTES_MAX),
            context: Value::Null,
            context_bytes: 0,
            error_type: None,
            failed_outcome: None,
        };
        labels.extend(&fields.rest);
        labels
    }

    /// Appends `rest` to both member sets and builds `context` again.
    fn extend(&mut self, rest: &[(String, String)]) {
        if rest.is_empty() && !self.context.is_null() {
            return;
        }
        for (name, value) in rest {
            if name == ERROR_TYPE_MEMBER {
                self.error_type = Some(error_type_label(value));
            } else if name == OUTCOME_FIELD {
                self.failed_outcome = (!completed_outcome(value)).then(|| error_type_label(value));
            }
            self.fields.push(name.clone(), Value::from(value.as_str()));
            self.context_fields
                .push(name.clone(), Value::from(value.as_str()));
        }
        self.context = serde_json::json!({
            "name": self.name,
            "fields": self.context_fields.object(),
        });
        self.context_bytes = serialized_bytes(&self.context);
    }
}

impl SpanLabels {
    /// The operation metrics' `error.type` label of the span's failure: its recorded
    /// `error.type`, else an `outcome` that is not a completion. `None` while neither was
    /// recorded.
    const fn failure(&self) -> Option<&'static str> {
        match self.error_type {
            Some(label) => Some(label),
            None => self.failed_outcome,
        }
    }
}

/// Field members of one JSON object, at most `maximum` bytes of members as `serde_json`
/// writes them, without the enclosing braces, with the count of the members left out at
/// that bound.
///
/// A member is kept whole or left out, so the object stays well formed. A member pushed
/// again under a name the set holds replaces the value in its place, as a JSON reader keeps
/// the last of two members of one name.
#[derive(Clone, Debug)]
struct BoundedMembers {
    members: Map<String, Value>,
    bytes: usize,
    maximum: usize,
    left_out: u64,
}

impl BoundedMembers {
    /// An empty set bounded at `maximum` bytes.
    fn new(maximum: usize) -> Self {
        Self {
            members: Map::new(),
            bytes: 0,
            maximum,
            left_out: 0,
        }
    }

    /// Appends the member `name`: `value` when it fits whole, and counts it left out when
    /// it does not.
    fn push(&mut self, name: String, value: Value) {
        let bytes = serialized_bytes(&value);
        self.push_measured(name, value, bytes);
    }

    /// [`Self::push`] of a `value` `serde_json` writes in `value_bytes` bytes.
    fn push_measured(&mut self, name: String, value: Value, value_bytes: usize) {
        let member = member_bytes(&name, value_bytes);
        let bytes = match self.members.get(&name) {
            Some(held) => self.bytes - member_bytes(&name, serialized_bytes(held)) + member,
            None => self.bytes + usize::from(!self.members.is_empty()) * ",".len() + member,
        };
        if bytes > self.maximum {
            self.left_out = self.left_out.saturating_add(1);
            return;
        }
        self.bytes = bytes;
        self.members.insert(name, value);
    }

    /// The members, then [`FIELDS_LEFT_OUT_MEMBER`] when a member was left out.
    fn object(&self) -> Map<String, Value> {
        self.clone().into_object()
    }

    /// [`Self::object`], taking the members.
    fn into_object(self) -> Map<String, Value> {
        let mut members = self.members;
        if self.left_out > 0 {
            members.insert(
                FIELDS_LEFT_OUT_MEMBER.to_owned(),
                Value::String(self.left_out.to_string()),
            );
        }
        members
    }
}

/// Bytes `serde_json` writes for the member `name` holding a value of `value_bytes` bytes.
fn member_bytes(name: &str, value_bytes: usize) -> usize {
    serialized_bytes(&Value::from(name)) + ":".len() + value_bytes
}

/// Bytes of `value` as `serde_json` writes it, without whitespace.
fn serialized_bytes(value: &Value) -> usize {
    value.to_string().len()
}

/// The fields one event or span recorded: its message, the two labels the codebase
/// files diagnostics under, and everything else as text, in the order they were recorded.
#[derive(Debug, Default)]
struct RecordedFields {
    message: String,
    component: String,
    operation: String,
    rest: Vec<(String, String)>,
}

impl RecordedFields {
    /// The remaining fields as a JSON object, always well formed.
    #[cfg(test)]
    fn rendered(&self) -> String {
        record_fields(
            self.rest
                .iter()
                .map(|(name, value)| (name.clone(), Value::from(value.as_str()))),
            &[],
        )
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

/// Installs the panic hook that records a panic before the default hook prints it.
///
/// A detached server's panic reaches nobody otherwise: its standard error is a file at
/// best, and the default hook writes there alone. The installed hook emits one `ERROR`
/// event carrying the payload and the source location, through whatever subscriber the
/// panicking thread runs under, then publishes the table of operations in flight as one
/// `WARN` record with the reason `panic`, as [`crate::warn_in_flight`] does, and hands the
/// panic to the hook that was installed before it. Installing twice chains the hooks, so a
/// second call records each panic twice; the server installs it once, before it serves.
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
        crate::flight::warn_in_flight("panic");
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
