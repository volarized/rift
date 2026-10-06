//! The table of operations in flight: every operation, lock wait, and held lock that has
//! opened and not yet closed.
//!
//! Stderr and the store receive an operation when it closes, so neither shows the work a
//! deadline failure needs to see: the work still running. The table keeps that work. An
//! entry joins when its span opens and leaves when the span closes, and the runtime
//! publishes the table as one record on demand ([`publish_in_flight`]) and, from the stall
//! report's own task, for each entry open past `[logs] stall_delay`. A held lock declared
//! [`lifelong`](crate::Lock::lifelong) is listed with that mark and left out of the stall
//! report: its holder keeps it for as long as the holder runs.
//!
//! The table holds at most [`OPERATIONS_IN_FLIGHT_MAX`] entries, each label cut at
//! [`LOG_LABEL_BYTES_MAX`]; an entry that finds the table full is not tracked, and every
//! table record carries the count of those refused since the process started.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Metadata, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::capture::now_ms;
use crate::measurement::monotonic_now;
use crate::metrics::{Counter, ObservableUpDownCounter, ObservationGuard, SCOPE};
use crate::record::{LOG_LABEL_BYTES_MAX, bounded};

/// Entries the table of operations in flight holds, at most. An operation that opens while
/// the table is full is not tracked, and the table counts it.
pub const OPERATIONS_IN_FLIGHT_MAX: usize = 1_024;
/// Bytes of the `operations` field one table record lists, at most: the oldest entries that
/// fit are listed, and the rest are counted in `left_out`.
const OPERATIONS_LISTED_BYTES_MAX: usize = 4 << 10;
/// `operation.active`: the entries the table holds open now, by `span.name`, read each time
/// the meter collects.
static OPERATION_ACTIVE: ObservableUpDownCounter<1> =
    ObservableUpDownCounter::declare(SCOPE, "operation.active", "{operation}", &["span.name"]);
/// `operation.untracked`: the entries the table refused at [`OPERATIONS_IN_FLIGHT_MAX`].
pub(crate) static OPERATION_UNTRACKED: Counter<0> =
    Counter::declare(SCOPE, "operation.untracked", "{operation}", &[]);
/// The name of the span a contended lock wait opens.
pub(crate) const LOCK_WAIT_SPAN: &str = "lock.wait";
/// The name of the span a held lock keeps open until its guard drops.
pub(crate) const LOCK_HELD_SPAN: &str = "lock.held";
/// The span field that names a lock.
pub(crate) const LOCK_NAME_FIELD: &str = "lock.name";
/// The span field that marks a hold its holder keeps for as long as it runs.
const LIFELONG_FIELD: &str = "lifelong";
/// The span field that names the work an operation runs, as `worker.run` names the request
/// operation it runs on a worker.
const WORK_FIELD: &str = "work";
/// The member of a listed entry that holds the fields recorded on its span after it opened.
const RECORDED_FIELD: &str = "fields";

/// What an entry of the table is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FlightKind {
    /// An operation: a span that carries an `operation` field.
    Operation,
    /// A wait for a lock.
    Wait,
    /// A lock held until its guard drops.
    Held,
}

impl FlightKind {
    /// The kind of the span `metadata` describes, or `None` for a span the table skips.
    fn of(metadata: &Metadata<'_>) -> Option<Self> {
        match metadata.name() {
            LOCK_WAIT_SPAN => Some(Self::Wait),
            LOCK_HELD_SPAN => Some(Self::Held),
            _ if metadata.fields().field("operation").is_some() => Some(Self::Operation),
            _ => None,
        }
    }

    /// The spelling a table record carries.
    const fn label(self) -> &'static str {
        match self {
            Self::Operation => "operation",
            Self::Wait => "wait",
            Self::Held => "held",
        }
    }
}

/// One entry: what opened, under which parent, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FlightEntry {
    pub(crate) name: &'static str,
    pub(crate) component: String,
    pub(crate) kind: FlightKind,
    pub(crate) lock: String,
    /// The span's `work` field, empty when it has none.
    pub(crate) work: String,
    pub(crate) parent: Option<&'static str>,
    pub(crate) started_at_ms: i64,
    pub(crate) started: Duration,
    /// Fields recorded on the span after it opened, such as a child's `pid`, by field
    /// name, each value cut at [`LOG_LABEL_BYTES_MAX`]; a field recorded again keeps its
    /// latest value. The span's callsite declares the names, so their count is fixed.
    pub(crate) recorded: BTreeMap<&'static str, String>,
    /// Kept by its holder for as long as the holder runs: listed, never reported stalled.
    pub(crate) lifelong: bool,
    stall_reported: bool,
}

impl FlightEntry {
    /// An entry of `kind` named `name`, opened at `started` on the monotonic clock and
    /// `started_at_ms` on the log clock.
    pub(crate) fn opened(
        name: &'static str,
        kind: FlightKind,
        parent: Option<&'static str>,
        started: Duration,
        started_at_ms: i64,
    ) -> Self {
        Self {
            name,
            component: String::new(),
            kind,
            lock: String::new(),
            work: String::new(),
            recorded: BTreeMap::new(),
            parent,
            started_at_ms,
            started,
            lifelong: false,
            stall_reported: false,
        }
    }

    /// The entry as one member of a table record's `operations` list: its name, kind, and
    /// age at `now`, then its component, lock, work, and parent when it has them, and
    /// `"lifelong": true` for a lifelong hold.
    fn listed(&self, now: Duration) -> Value {
        let mut member = Map::new();
        member.insert("operation".to_owned(), Value::from(self.name));
        member.insert("kind".to_owned(), Value::from(self.kind.label()));
        let age_ms =
            u64::try_from(now.saturating_sub(self.started).as_millis()).unwrap_or(u64::MAX);
        member.insert("age_ms".to_owned(), Value::from(age_ms));
        member.insert("started_at_ms".to_owned(), Value::from(self.started_at_ms));
        for (key, value) in [
            ("component", &self.component),
            (LOCK_NAME_FIELD, &self.lock),
            (WORK_FIELD, &self.work),
        ] {
            if !value.is_empty() {
                member.insert(key.to_owned(), Value::from(value.as_str()));
            }
        }
        if let Some(parent) = self.parent {
            member.insert("parent".to_owned(), Value::from(parent));
        }
        if self.lifelong {
            member.insert(LIFELONG_FIELD.to_owned(), Value::from(true));
        }
        if !self.recorded.is_empty() {
            let fields = self
                .recorded
                .iter()
                .map(|(name, value)| ((*name).to_owned(), Value::from(value.as_str())))
                .collect();
            member.insert(RECORDED_FIELD.to_owned(), Value::Object(fields));
        }
        Value::Object(member)
    }
}

/// The entries, keyed by span identity, and the count of entries refused at the bound.
#[derive(Debug, Default)]
struct FlightEntries {
    open: HashMap<u64, FlightEntry>,
    untracked: u64,
}

/// The table one runtime or scoped recorder keeps.
#[derive(Debug, Default)]
pub(crate) struct FlightTable {
    entries: Mutex<FlightEntries>,
}

/// One publication of the table: the entries listed, oldest first, and the counts a reader
/// needs to know the list is whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FlightListing {
    /// Entries open when the table was read, or, for a stall report, entries newly past
    /// the stall delay.
    pub(crate) in_flight: u64,
    /// Entries that did not fit [`OPERATIONS_LISTED_BYTES_MAX`].
    pub(crate) left_out: u64,
    /// Entries refused at [`OPERATIONS_IN_FLIGHT_MAX`] since the table started.
    pub(crate) untracked: u64,
    /// The listed entries, as a JSON array.
    pub(crate) operations: String,
}

impl fmt::Display for FlightListing {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.operations)
    }
}

impl FlightTable {
    fn lock(&self) -> MutexGuard<'_, FlightEntries> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds `entry` under `identity`, or counts it untracked when the table is full: in the
    /// table, and in `operation.untracked` through the instrument the meter's install built,
    /// since the join runs inside the table's `tracing` layer.
    pub(crate) fn join(&self, identity: u64, entry: FlightEntry) {
        let mut entries = self.lock();
        if entries.open.len() >= OPERATIONS_IN_FLIGHT_MAX {
            entries.untracked = entries.untracked.saturating_add(1);
            drop(entries);
            OPERATION_UNTRACKED.add_built([], 1);
            return;
        }
        entries.open.insert(identity, entry);
    }

    /// The open entries, counted by span name: at most [`OPERATIONS_IN_FLIGHT_MAX`] names.
    fn active(&self) -> HashMap<&'static str, u64> {
        let entries = self.lock();
        let mut active = HashMap::new();
        for entry in entries.open.values() {
            *active.entry(entry.name).or_insert(0) += 1;
        }
        active
    }

    /// Applies `values`, recorded on the span of the entry under `identity` after it opened,
    /// to that entry: `component`, the lock name, and `work` replace what it carries, and
    /// every other field joins its recorded fields. An identity the table does not track
    /// changes nothing.
    fn record(&self, identity: u64, values: &tracing::span::Record<'_>) {
        let mut entries = self.lock();
        let Some(entry) = entries.open.get_mut(&identity) else {
            return;
        };
        let mut fields = EntryFields::recorded_later();
        values.record(&mut fields);
        for (target, value) in [
            (&mut entry.component, fields.component),
            (&mut entry.lock, fields.lock),
            (&mut entry.work, fields.work),
        ] {
            if !value.is_empty() {
                *target = value;
            }
        }
        entry.recorded.extend(fields.recorded);
    }

    /// Removes the entry under `identity`, if the table tracked it.
    pub(crate) fn leave(&self, identity: u64) {
        self.lock().open.remove(&identity);
    }

    /// The parent operation of the oldest held entry of the lock `lock`: the operation that
    /// holds it.
    pub(crate) fn holder_of(&self, lock: &str) -> Option<&'static str> {
        let entries = self.lock();
        entries
            .open
            .values()
            .filter(|entry| entry.kind == FlightKind::Held && entry.lock == lock)
            .min_by_key(|entry| entry.started)
            .and_then(|entry| entry.parent)
    }

    /// Every open entry, oldest first, with the ages they have at `now`.
    pub(crate) fn listing(&self, now: Duration) -> FlightListing {
        let (selected, untracked) = {
            let entries = self.lock();
            (
                entries.open.values().cloned().collect::<Vec<_>>(),
                entries.untracked,
            )
        };
        listed(selected, untracked, now)
    }

    /// The entries open for `stall_delay` or longer at `now` that no earlier call reported,
    /// marked reported, oldest first; `None` when there are none. A lifelong entry is never
    /// selected.
    pub(crate) fn stalled(&self, now: Duration, stall_delay: Duration) -> Option<FlightListing> {
        let (selected, untracked) = {
            let mut entries = self.lock();
            let mut selected = Vec::new();
            for entry in entries.open.values_mut() {
                if !entry.lifelong
                    && !entry.stall_reported
                    && now.saturating_sub(entry.started) >= stall_delay
                {
                    entry.stall_reported = true;
                    selected.push(entry.clone());
                }
            }
            (selected, entries.untracked)
        };
        (!selected.is_empty()).then(|| listed(selected, untracked, now))
    }
}

/// `entries` listed oldest first within [`OPERATIONS_LISTED_BYTES_MAX`], the rest counted.
fn listed(mut entries: Vec<FlightEntry>, untracked: u64, now: Duration) -> FlightListing {
    entries.sort_by_key(|entry| entry.started);
    let mut operations = String::from("[");
    let mut listed = 0_usize;
    for entry in &entries {
        let member = entry.listed(now).to_string();
        let separator = usize::from(listed > 0);
        if operations.len() + separator + member.len() + 1 > OPERATIONS_LISTED_BYTES_MAX {
            break;
        }
        if separator == 1 {
            operations.push(',');
        }
        operations.push_str(&member);
        listed += 1;
    }
    operations.push(']');
    FlightListing {
        in_flight: entries.len() as u64,
        left_out: (entries.len() - listed) as u64,
        untracked,
        operations,
    }
}

/// The fields an entry takes from its span: `component`, the lock name, `work`, and the
/// lifelong mark; and, from a span's later records alone, every other field.
#[derive(Default)]
struct EntryFields {
    component: String,
    lock: String,
    work: String,
    lifelong: bool,
    /// Whether fields other than the four above are kept in `recorded`: on a later
    /// record, not at the opening, whose fields every entry would otherwise copy.
    keeps_others: bool,
    recorded: BTreeMap<&'static str, String>,
}

impl EntryFields {
    /// Fields of a record made after the span opened.
    fn recorded_later() -> Self {
        Self {
            keeps_others: true,
            ..Self::default()
        }
    }

    fn record(&mut self, field: &Field, value: &str) {
        match field.name() {
            "component" => self.component = bounded(value, LOG_LABEL_BYTES_MAX),
            LOCK_NAME_FIELD => self.lock = bounded(value, LOG_LABEL_BYTES_MAX),
            WORK_FIELD => self.work = bounded(value, LOG_LABEL_BYTES_MAX),
            LIFELONG_FIELD => {}
            other if self.keeps_others => {
                self.recorded
                    .insert(other, bounded(value, LOG_LABEL_BYTES_MAX));
            }
            _ => {}
        }
    }
}

impl Visit for EntryFields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value);
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == LIFELONG_FIELD {
            self.lifelong = value;
        } else if self.keeps_others {
            self.record(field, if value { "true" } else { "false" });
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if self.keeps_others || matches!(field.name(), "component" | LOCK_NAME_FIELD | WORK_FIELD) {
            self.record(field, &format!("{value:?}"));
        }
    }
}

/// The `tracing` layer that keeps the table: unfiltered, so an operation below every
/// filter is still in flight.
#[derive(Clone, Debug)]
pub(crate) struct FlightLayer {
    table: Arc<FlightTable>,
}

impl FlightLayer {
    /// The layer over `table`.
    pub(crate) const fn new(table: Arc<FlightTable>) -> Self {
        Self { table }
    }
}

impl<S> Layer<S> for FlightLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let Some(kind) = FlightKind::of(attributes.metadata()) else {
            return;
        };
        let mut fields = EntryFields::default();
        attributes.record(&mut fields);
        // A lifelong hold opens as a root so the operation that took the lock can close; it
        // names the span it was opened in as its parent. Any other root has none.
        let parent = if fields.lifelong {
            context.lookup_current().map(|current| current.name())
        } else {
            context
                .span(id)
                .and_then(|span| span.parent())
                .map(|parent| parent.name())
        };
        let mut entry = FlightEntry::opened(
            attributes.metadata().name(),
            kind,
            parent,
            monotonic_now(),
            now_ms(),
        );
        entry.component = fields.component;
        entry.lock = fields.lock;
        entry.work = fields.work;
        entry.lifelong = fields.lifelong;
        self.table.join(id.into_u64(), entry);
    }

    /// A field recorded after the span opened, such as the `pid` of a child it started,
    /// joins the span's entry.
    fn on_record(&self, id: &Id, values: &tracing::span::Record<'_>, _context: Context<'_, S>) {
        self.table.record(id.into_u64(), values);
    }

    fn on_close(&self, id: Id, context: Context<'_, S>) {
        if context
            .metadata(&id)
            .and_then(|metadata| FlightKind::of(metadata))
            .is_some()
        {
            self.table.leave(id.into_u64());
        }
    }
}

/// Reports the entries `table` holds open in `operation.active` each time the meter
/// collects, until the guard drops; `None` when the process installed no meter. The read
/// holds the table weakly, so a table its owner dropped reports nothing.
pub(crate) fn observe_active(table: &Arc<FlightTable>) -> Option<ObservationGuard> {
    let table = Arc::downgrade(table);
    OPERATION_ACTIVE.observe(move |observation| {
        if let Some(table) = table.upgrade() {
            for (name, count) in table.active() {
                observation.observe([name], count);
            }
        }
    })
}

/// Runs `read` against the table of the thread's current dispatcher, when it keeps one.
pub(crate) fn with_table<Answer>(read: impl FnOnce(&FlightTable) -> Answer) -> Option<Answer> {
    let mut read = Some(read);
    tracing::dispatcher::get_default(|dispatch| {
        let layer = dispatch.downcast_ref::<FlightLayer>()?;
        read.take().map(|read| read(&layer.table))
    })
}

/// Publishes the table of operations in flight as one record, naming `reason`.
///
/// The record is an `INFO` event with the target `rift_tracing::flight` and the message
/// `operations in flight`. Its fields are `reason`; `in_flight`, the entries open;
/// `left_out`, the entries past the record's field bound; `untracked`, the operations
/// refused at [`OPERATIONS_IN_FLIGHT_MAX`]; and `operations`, a JSON array of the open
/// entries, oldest first. Each entry names its `operation`, its `kind` (`operation`,
/// `wait`, or `held`), its `age_ms`, its `started_at_ms`, and its `component`,
/// `lock.name`, and `parent` operation when it has them; a hold declared
/// [`lifelong`](crate::Lock::lifelong) also carries `"lifelong": true`. An entry whose span
/// recorded fields after it opened, such as the `pid` of a child it started, lists them,
/// latest value each, in the object `fields`.
///
/// A thread whose dispatcher keeps no table publishes nothing.
///
/// ```
/// rift_tracing::traced!(component = "mcp", operation = "server.stop", {
///     rift_tracing::publish_in_flight("stop");
/// });
/// ```
pub fn publish_in_flight(reason: &'static str) {
    if let Some(listing) = with_table(|table| table.listing(monotonic_now())) {
        tracing::info!(
            target: "rift_tracing::flight",
            reason,
            in_flight = listing.in_flight,
            left_out = listing.left_out,
            untracked = listing.untracked,
            operations = %listing,
            "operations in flight"
        );
    }
}

/// Publishes the table of operations in flight as [`publish_in_flight`] does, as one `WARN`
/// record.
///
/// A deadline that expired is a warning, and a process whose filter admits warnings alone,
/// such as one started under `RUST_LOG=warn`, keeps the table it publishes then: on stderr,
/// and in the store when the drain writes before the process exits.
///
/// ```
/// rift_tracing::traced!(component = "mcp", operation = "server.stop", {
///     rift_tracing::warn_in_flight("stop deadline");
/// });
/// ```
pub fn warn_in_flight(reason: &'static str) {
    if let Some(listing) = with_table(|table| table.listing(monotonic_now())) {
        tracing::warn!(
            target: "rift_tracing::flight",
            reason,
            in_flight = listing.in_flight,
            left_out = listing.left_out,
            untracked = listing.untracked,
            operations = %listing,
            "operations in flight"
        );
    }
}

/// Publishes the entries of `table` open for `stall_delay` at `now` that no earlier tick
/// reported, lifelong holds left out, as one `WARN` record with the fields of
/// [`publish_in_flight`] and the reason `stall_delay`.
pub(crate) fn publish_stalled(table: &FlightTable, now: Duration, stall_delay: Duration) {
    if let Some(listing) = table.stalled(now, stall_delay) {
        tracing::warn!(
            target: "rift_tracing::flight",
            reason = "stall_delay",
            in_flight = listing.in_flight,
            left_out = listing.left_out,
            untracked = listing.untracked,
            operations = %listing,
            "operations in flight past the stall delay"
        );
    }
}

/// The stall report's tick is `stall_delay` divided by this, within [`STALL_TICK_MIN`] and
/// [`STALL_TICK_MAX`], so an entry is reported at most a quarter of its delay late.
const STALL_TICKS_PER_DELAY: u32 = 4;
/// The shortest stall report tick: a quarter of the shortest `[logs] stall_delay`, one
/// second.
pub(crate) const STALL_TICK_MIN: Duration = Duration::from_millis(250);
/// The longest stall report tick, reached from a `[logs] stall_delay` of 20 s: a longer
/// delay is still reported at most this late.
pub(crate) const STALL_TICK_MAX: Duration = Duration::from_secs(5);

/// The tick of a stall report under `stall_delay`: a quarter of it, clamped to
/// [`STALL_TICK_MIN`] and [`STALL_TICK_MAX`].
///
/// An entry that crosses `stall_delay` right after a tick is reported on the next one, so
/// a report comes at most one tick, plus the runtime's scheduling delay, after the entry
/// crossed `stall_delay`.
pub(crate) fn stall_tick(stall_delay: Duration) -> Duration {
    (stall_delay / STALL_TICKS_PER_DELAY).clamp(STALL_TICK_MIN, STALL_TICK_MAX)
}

/// The running stall report: the task that reads the table every [`stall_tick`], and the
/// token that stops it.
#[derive(Debug)]
pub(crate) struct StallReport {
    cancel: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl StallReport {
    /// Starts reporting the entries of `table` open past `stall_delay` on the current Tokio
    /// runtime, aged on the monotonic clock.
    pub(crate) fn spawn(table: Arc<FlightTable>, stall_delay: Duration) -> Self {
        Self::spawn_with_clock(table, stall_delay, monotonic_now)
    }

    /// Starts the report with `now` as the clock entries age on.
    pub(crate) fn spawn_with_clock(
        table: Arc<FlightTable>,
        stall_delay: Duration,
        now: impl Fn() -> Duration + Send + 'static,
    ) -> Self {
        let cancel = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(report_stalls(table, stall_delay, now, cancel.clone()));
        Self { cancel, task }
    }

    /// Stops the report and waits for its task to end: the task holds no await but its
    /// tick, so it ends at the cancellation.
    pub(crate) async fn stop(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

/// Publishes the stalled entries of `table` every [`stall_tick`] until `cancel` fires.
async fn report_stalls(
    table: Arc<FlightTable>,
    stall_delay: Duration,
    now: impl Fn() -> Duration,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut ticks = tokio::time::interval(stall_tick(stall_delay));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            _ = ticks.tick() => {}
        }
        publish_stalled(&table, now(), stall_delay);
    }
}

#[cfg(test)]
mod tests;
