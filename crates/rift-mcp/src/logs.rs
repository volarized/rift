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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rift_core::causes;
use rift_index::{LOG_BATCH_RECORDS_MAX, LogRecord, LogStore};
use rift_protocol::configuration::LogsConfiguration;
use tokio::sync::mpsc::{self, Receiver, Sender, error::TrySendError};
use tokio::sync::{Notify, watch};
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
/// Longest a `rift://logs` read waits for the drain to finish with the records the sink has
/// already taken. A read past this answers with what the store holds: a log read never fails,
/// and never hangs, because the log lane is slow.
pub const LOG_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
/// Attempts one batch gets before the drain gives it up. A batch that fails
/// every attempt is dropped and counted, never retried forever: the queue
/// behind it keeps filling while this one waits.
const LOG_WRITE_ATTEMPTS_MAX: u32 = 5;
/// Wall-clock span between two attempts at the same batch.
const LOG_WRITE_RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Bytes of one span's own fields the close record keeps, at most. A longer set
/// is cut at a character boundary, the way a message past
/// [`rift_index::LOG_MESSAGE_BYTES_MAX`] is, and the bound leaves the close
/// record's `span` and `elapsed_ms` members room under
/// [`rift_index::LOG_FIELDS_BYTES_MAX`].
const SPAN_FIELDS_BYTES_MAX: usize = 1 << 10;

tokio::task_local! {
    /// Set while the drain writes one batch. A record the write itself produces is not a
    /// record of this workspace's own work: `[logs] capture` accepts any filter that parses,
    /// so a filter admitting the storage driver's targets would otherwise make each batch
    /// written produce the records of the next one. The marker is task-local and the write
    /// runs on the drain's task alone, so an event another task emits during the write is
    /// still recorded.
    static WRITING_BATCH: ();
}

/// Whether this event came out of the drain's own write.
fn inside_the_drains_write() -> bool {
    WRITING_BATCH.try_with(|()| ()).is_ok()
}

/// One record on its way to the drain, under the sequence the sink stamped on it.
///
/// The sequence is what a read waits on. A count cannot serve: a full queue drops the newest
/// record while older ones are still queued, so the number of records the lane has finished
/// with says nothing about which ones.
#[derive(Debug)]
struct QueuedRecord {
    sequence: u64,
    record: LogRecord,
}

/// How far the drain has got through the sequence the sink stamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LaneProgress {
    /// Highest sequence the drain has written. Every lower sequence is written or dropped,
    /// because the queue is first in, first out.
    written_through: u64,
    /// Records the lane is finished with: written by the drain, or dropped by a full queue.
    finished: u64,
}

/// What the log lane has taken, and how far it has got.
///
/// A `rift://logs` read stamps its target from `accepted` on entry and waits for the drain to
/// write through it, so the read sees the records its own request produced rather than
/// whatever the drain's timer had committed by then. Without it the drain's
/// [`LOG_FLUSH_INTERVAL`] is a window in which a caller reads back its own missing diagnostic.
#[derive(Debug)]
pub struct LogSettlement {
    accepted: AtomicU64,
    progress: watch::Sender<LaneProgress>,
    flush: Notify,
    draining: AtomicBool,
}

impl LogSettlement {
    /// Stamps one record on its way into the queue and answers its sequence. Stamped before
    /// the send, so a read taken immediately after a traced call cannot observe a sequence
    /// that misses it.
    fn accept(&self) -> u64 {
        self.accepted.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// One record the queue had no room for. It is finished with, and no sequence advances:
    /// a record the drain never sees is one no read can wait for.
    fn finish_dropped(&self) {
        self.progress.send_modify(|progress| progress.finished += 1);
    }

    /// One written batch: `count` records finished, the highest sequence among them written.
    fn finish_written(&self, count: u64, written_through: u64) {
        self.progress.send_modify(|progress| {
            progress.finished += count;
            progress.written_through = progress.written_through.max(written_through);
        });
    }

    /// Whether a read stamped at `target` may proceed.
    ///
    /// Either the drain has written through that sequence, or the lane has finished with
    /// everything it has taken, which says the record at `target` was dropped and no wait
    /// will produce it.
    fn reached(&self, progress: LaneProgress, target: u64) -> bool {
        progress.written_through >= target
            || progress.finished >= self.accepted.load(Ordering::SeqCst)
    }

    /// Waits for the drain to write through the sequence this read stamped on entry.
    ///
    /// Returns at once when no drain is running and when the lane is already there. Past
    /// [`LOG_SETTLE_TIMEOUT`] the read proceeds with whatever the store holds.
    async fn settle_for_read(&self) {
        if !self.draining.load(Ordering::SeqCst) {
            return;
        }
        let target = self.accepted.load(Ordering::SeqCst);
        let mut progress = self.progress.subscribe();
        if self.reached(*progress.borrow_and_update(), target) {
            return;
        }
        self.flush.notify_one();
        let reached = async {
            while progress.changed().await.is_ok() {
                if self.reached(*progress.borrow_and_update(), target) {
                    return;
                }
            }
        };
        let _ = tokio::time::timeout(LOG_SETTLE_TIMEOUT, reached).await;
    }
}

/// The settlement of the lane this process installed, when it installed one.
///
/// The sink is one process-wide `tracing` layer, so its settlement is process-wide too. A
/// process that records nothing - every command but the foreground server - leaves this empty
/// and its log reads wait for nothing.
static INSTALLED_SETTLEMENT: Mutex<Option<Arc<LogSettlement>>> = Mutex::new(None);

/// Waits for this process's log lane to write through the sequence it has stamped, if it has a
/// lane. Every `rift://logs` read calls this before it opens the store.
///
/// # Panics
///
/// Panics when the installed lane's lock is poisoned, which needs a panic while it is held:
/// the guard covers one `Arc` clone and nothing that can fail.
pub async fn settle_for_read() {
    let installed = INSTALLED_SETTLEMENT
        .lock()
        .expect("the installed settlement is not poisoned")
        .clone();
    if let Some(settlement) = installed {
        settlement.settle_for_read().await;
    }
}

/// The `tracing` layer that copies admitted events into the queue.
///
/// Cloning shares one queue: the layer is installed once, and a clone held for
/// a test observes the same drops.
#[derive(Clone, Debug)]
pub struct LogSink {
    sender: Sender<QueuedRecord>,
    dropped: Arc<AtomicU64>,
    settlement: Arc<LogSettlement>,
}

impl LogSink {
    /// How many records the queue has dropped for being full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Queues one record, counting a drop rather than waiting for room.
    ///
    /// The record takes its sequence before the send, and a send that finds no room finishes
    /// it again: a read waiting on the sequence must never wait for a record no drain sees.
    fn send(&self, record: LogRecord) {
        if inside_the_drains_write() {
            return;
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
}

/// The queue's reading end, and the task that writes it into the store.
#[derive(Debug)]
pub struct LogDrain {
    receiver: Receiver<QueuedRecord>,
    dropped: Arc<AtomicU64>,
    settlement: Arc<LogSettlement>,
}

impl LogDrain {
    /// One queued record, without waiting; for a test that reads the queue without a store.
    #[cfg(test)]
    pub(crate) fn try_recv_record(&mut self) -> Result<LogRecord, mpsc::error::TryRecvError> {
        self.receiver.try_recv().map(|queued| queued.record)
    }

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
        self.settlement.draining.store(true, Ordering::SeqCst);
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
                () = self.settlement.flush.notified(), if !closing => {}
                () = tokio::time::sleep(LOG_FLUSH_INTERVAL), if !closing => {}
                else => {}
            }
            while batch.len() < LOG_BATCH_RECORDS_MAX {
                match self.receiver.try_recv() {
                    Ok(queued) => batch.push(queued),
                    Err(_) => break,
                }
            }
            self.write_turn(&store, &mut batch, retention_records).await;
        }
        self.write_turn(&store, &mut batch, retention_records).await;
        self.settlement.draining.store(false, Ordering::SeqCst);
    }

    /// Writes one batch, retrying a refused write before giving it up.
    ///
    /// In-process writers queue before reaching `SQLite`. A refusal can still come from
    /// another process or an operating failure. A batch that still fails after
    /// [`LOG_WRITE_ATTEMPTS_MAX`] is counted as dropped, which the next batch
    /// records: the queue behind this one is still filling.
    /// Writes one batch and reports how far through the sequence the lane now is.
    ///
    /// The highest sequence in the batch is written through, and every lower one is written or
    /// was dropped, because the queue is first in, first out.
    async fn write_turn(
        &self,
        store: &LogStore,
        batch: &mut Vec<QueuedRecord>,
        retention_records: u64,
    ) {
        self.note_drops(batch);
        if batch.is_empty() {
            return;
        }
        let written_through = batch
            .iter()
            .map(|queued| queued.sequence)
            .max()
            .unwrap_or(0);
        let records: Vec<LogRecord> = batch.iter().map(|queued| queued.record.clone()).collect();
        self.write_batch(store, &records, retention_records).await;
        self.settlement
            .finish_written(batch.len() as u64, written_through);
        batch.clear();
    }

    async fn write_batch(&self, store: &LogStore, batch: &[LogRecord], retention_records: u64) {
        for attempt in 1..=LOG_WRITE_ATTEMPTS_MAX {
            match WRITING_BATCH
                .scope((), store.append(batch, retention_records))
                .await
            {
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
    fn note_drops(&self, batch: &mut Vec<QueuedRecord>) {
        if batch.len() == LOG_BATCH_RECORDS_MAX {
            return;
        }
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        if dropped == 0 {
            return;
        }
        // The drain mints this one, so it carries no sequence of its own: sequence zero is
        // below every stamped record and never raises what the lane has written through.
        batch.push(QueuedRecord {
            sequence: 0,
            record: LogRecord::new(
                now_ms(),
                "warn",
                "rift_mcp::logs",
                "logs",
                "logs.drain",
                "the log queue was full and dropped records",
                &format!("{{\"dropped\":{dropped}}}"),
            ),
        });
    }
}

/// Builds the layer and its drain, sharing one bounded queue and one settlement.
///
/// # Panics
///
/// Panics when the installed lane's lock is poisoned, which needs a panic while it is held:
/// the guard covers one `Arc` replacement and nothing that can fail.
#[must_use]
pub fn log_capture() -> (LogSink, LogDrain) {
    let (sender, receiver) = mpsc::channel(LOG_QUEUE_RECORDS);
    let dropped = Arc::new(AtomicU64::new(0));
    let settlement = Arc::new(LogSettlement {
        accepted: AtomicU64::new(0),
        progress: watch::Sender::new(LaneProgress::default()),
        flush: Notify::new(),
        draining: AtomicBool::new(false),
    });
    // A `rift://logs` read reaches the lane through this, because the sink is installed as one
    // process-wide layer and the reading server never holds it. The newest lane wins, which is
    // what a test building a second one means.
    INSTALLED_SETTLEMENT
        .lock()
        .expect("the installed settlement is not poisoned")
        .replace(Arc::clone(&settlement));
    (
        LogSink {
            sender,
            dropped: Arc::clone(&dropped),
            settlement: Arc::clone(&settlement),
        },
        LogDrain {
            receiver,
            dropped,
            settlement,
        },
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
/// it, with its remaining fields and the moment the span opened.
///
/// The moment is what lets a closing span record how long it took. Stderr gets
/// that from the fmt layer's own close line, which no other layer ever sees, so
/// a store fed by events alone could say a rebuild happened and never how long
/// it ran - the first question a wedged workspace raises.
///
/// `fields` carries every other field the span recorded, as JSON object members,
/// so the close record says what the span did and not only that it ended. It is
/// cut at [`SPAN_FIELDS_BYTES_MAX`] on a character boundary.
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
    /// The remaining fields as JSON object members, without the enclosing braces.
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
    for cause in causes(error) {
        let _ = write!(rendered, ": {cause}");
    }
    rendered
}

/// `value` cut to at most `maximum` UTF-8 bytes, at a character boundary.
fn bounded(value: &str, maximum: usize) -> String {
    let mut cut = value.len().min(maximum);
    while !value.is_char_boundary(cut) {
        cut -= 1;
    }
    value[..cut].to_owned()
}

/// Bytes of a panic payload the recorded event keeps, at most.
pub const PANIC_PAYLOAD_BYTES_MAX: usize = 4 << 10;

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

    use super::{
        LOG_QUEUE_RECORDS, LOG_SETTLE_TIMEOUT, LogSettlement, PANIC_PAYLOAD_BYTES_MAX,
        RecordedFields, SPAN_FIELDS_BYTES_MAX, caused_by, install_panic_hook, log_capture,
        panic_payload, quoted,
    };

    /// One lane with `accepted` sequences stamped, the drain written through
    /// `written_through`, and a drain that is running or is not.
    fn settlement(accepted: u64, written_through: u64, draining: bool) -> LogSettlement {
        LogSettlement {
            accepted: std::sync::atomic::AtomicU64::new(accepted),
            progress: tokio::sync::watch::Sender::new(super::LaneProgress {
                written_through,
                finished: written_through,
            }),
            flush: tokio::sync::Notify::new(),
            draining: std::sync::atomic::AtomicBool::new(draining),
        }
    }

    /// A process that installed no drain waits for nothing. Every command but the foreground
    /// server records through no lane, and a log read there must not pay the bound.
    #[tokio::test(start_paused = true)]
    async fn a_read_waits_for_a_drain_that_is_not_running() {
        let started = tokio::time::Instant::now();
        settlement(4, 0, false).settle_for_read().await;
        assert_eq!(
            tokio::time::Instant::now(),
            started,
            "a read with no drain behind it waits for nothing"
        );
    }

    /// A settled queue costs a read nothing.
    #[tokio::test(start_paused = true)]
    async fn a_read_over_a_settled_queue_waits_for_nothing() {
        let started = tokio::time::Instant::now();
        settlement(4, 4, true).settle_for_read().await;
        assert_eq!(tokio::time::Instant::now(), started);
    }

    /// A drain that stops settling still lets the read through, at the bound. A log read never
    /// hangs because the log lane is stuck.
    #[tokio::test(start_paused = true)]
    async fn a_read_past_the_settle_bound_still_answers() {
        let started = tokio::time::Instant::now();
        settlement(4, 1, true).settle_for_read().await;
        assert_eq!(
            tokio::time::Instant::now() - started,
            LOG_SETTLE_TIMEOUT,
            "the read answers at the bound"
        );
    }

    /// A record the queue had no room for settles as it is accepted, so a read never waits out
    /// the bound for a record no drain will ever see.
    /// A record the drain's own write produced is not a record. `[logs] capture` accepts any
    /// filter that parses, and one admitting the storage driver made each batch written produce
    /// the records of the next, so the queue dropped the diagnostics the operator wanted.
    #[tokio::test]
    async fn a_record_the_drains_write_produced_is_not_queued() {
        let (sink, mut drain) = log_capture();
        super::WRITING_BATCH
            .scope((), async {
                sink.send(record("the storage driver wrote a batch"));
            })
            .await;
        assert!(
            queued(&mut drain).is_empty(),
            "the write's own record stays out of the queue it is draining"
        );
        assert_eq!(
            sink.settlement.accepted.load(Ordering::SeqCst),
            0,
            "a record the lane never takes advances no sequence"
        );
    }

    /// The marker is task-local, so a record another task emits while the drain writes is still
    /// recorded: the lane suppresses its own writes, not the workspace's work.
    #[tokio::test]
    async fn a_record_from_another_task_during_the_write_is_queued() {
        let (sink, mut drain) = log_capture();
        let emitter = sink.clone();
        super::WRITING_BATCH
            .scope((), async move {
                tokio::spawn(async move { emitter.send(record("the index left a file out")) })
                    .await
                    .expect("the emitting task runs");
            })
            .await;
        assert_eq!(
            queued(&mut drain).len(),
            1,
            "another task's record is unaffected by the drain's write"
        );
    }

    /// A read whose own record the full queue dropped still answers. The sequence it waits on
    /// never reaches the drain, so the lane's finished count is what releases it.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_record_does_not_hold_a_read() {
        let (sink, _drain) = log_capture();
        sink.settlement.draining.store(true, Ordering::SeqCst);
        for index in 0..=LOG_QUEUE_RECORDS {
            sink.send(record(&format!("record {index}")));
        }
        assert_eq!(
            sink.dropped(),
            1,
            "the queue holds one record less than sent"
        );
        let settlement = Arc::clone(&sink.settlement);
        settlement.finish_written(LOG_QUEUE_RECORDS as u64, LOG_QUEUE_RECORDS as u64);
        let started = tokio::time::Instant::now();
        settlement.settle_for_read().await;
        assert_eq!(
            tokio::time::Instant::now(),
            started,
            "the lane finished with every record it took, so the read does not wait"
        );
    }

    /// A read waits for the sequence it stamped, not for a count. A full queue drops the
    /// newest record while older ones wait, so a lane that has finished with as many records
    /// as the read stamped can still be holding the read's own record.
    #[tokio::test(start_paused = true)]
    async fn a_read_waits_for_its_own_sequence_not_for_a_count() {
        let lane = settlement(0, 0, true);
        lane.accepted.store(10, Ordering::SeqCst);
        lane.progress.send_modify(|progress| {
            progress.written_through = 4;
            progress.finished = 9;
        });
        let started = tokio::time::Instant::now();
        lane.settle_for_read().await;
        assert_eq!(
            tokio::time::Instant::now() - started,
            LOG_SETTLE_TIMEOUT,
            "nine records finished with does not mean sequence ten was written"
        );
    }
    use tracing::field::Visit;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    /// Drains what the queue currently holds, without a store.
    fn queued(drain: &mut super::LogDrain) -> Vec<rift_index::LogRecord> {
        let mut records = Vec::new();
        while let Ok(record) = drain.receiver.try_recv() {
            records.push(record.record);
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

    /// The one span-close record a case wrote.
    fn closed(records: Vec<rift_index::LogRecord>) -> rift_index::LogRecord {
        records
            .into_iter()
            .find(|record| record.fields().contains("\"span\":\"closed\""))
            .expect("a closed span is recorded")
    }

    /// A span records what it opened with and what it recorded later, so a reader of
    /// `rift://logs` sees the fields standard error shows.
    #[test]
    fn a_closing_span_records_the_fields_it_carried() {
        let (sink, mut drain) = log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "index.build",
                component = "index",
                operation = "index.rebuild",
                trigger = "filesystem",
                epoch = 7,
                changed_count = tracing::field::Empty,
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

    /// The hook is process-global: the thread panics under its own subscriber, so the
    /// event the hook emits lands in this case's queue and nowhere else.
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
        assert!(
            recorded.fields().contains("logs.rs"),
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

        let oversized: Box<dyn std::any::Any + Send> =
            Box::new("é".repeat(PANIC_PAYLOAD_BYTES_MAX));
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
    fn a_failure_renders_its_causes_after_its_own_text() {
        let refused = rift_core::Error::new(crate::election::ElectionFault::Storage {
            operation: "publish",
            path: std::path::PathBuf::from(".rift/server.json"),
            source: std::io::Error::other("disk full"),
        });
        assert_eq!(caused_by(&refused), ": disk full");
        assert_eq!(caused_by(&std::io::Error::other("disk full")), "");
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
