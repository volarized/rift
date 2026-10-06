use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use async_trait::async_trait;
use rift_tracing::{Counter, Histogram, PerformanceMeasurement};
use toasty_core::Schema;
use toasty_core::driver::operation::{Operation, Transaction, TransactionMode};
use toasty_core::driver::{
    Capability, ConnectContext, Connection as DriverConnection, Driver, ExecResponse,
};
use toasty_core::schema::db::{AppliedMigration, Migration};
use toasty_core::schema::diff;
use toasty_driver_sqlite::Sqlite;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::time::{Instant, interval, timeout_at};

use crate::database::DatabaseName;

/// The instrumentation scope of every instrument this crate declares: its Cargo package
/// name and version.
pub(crate) const SCOPE: rift_tracing::InstrumentScope =
    rift_tracing::InstrumentScope::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));

const CONNECTION_REAP_SPAN: Duration = Duration::from_millis(100);

/// `db.client.operation.duration`: one driver operation's execution on the worker, its
/// time in the queue left out.
static OPERATION_DURATION: Histogram<5> = Histogram::declare(
    SCOPE,
    "db.client.operation.duration",
    &[
        "db.system.name",
        "db.namespace",
        "db.operation.name",
        "db.response.status_code",
        "error.type",
    ],
);
/// The `db.system.name` of every database the worker serves.
const DB_SYSTEM: &str = "sqlite";
/// Statements recorded under their own text as `db.operation.name`, in place of the
/// driver's `raw_sql`: the busy timeout a checkout or the close sets, and the close's two
/// checkpoints. A raw statement whose text starts with one of these is recorded under it,
/// so the set of names stays closed whatever value the statement carries.
const NAMED_STATEMENTS: [&str; 3] = [
    "PRAGMA busy_timeout",
    "PRAGMA wal_checkpoint(NOOP)",
    "PRAGMA wal_checkpoint(TRUNCATE)",
];
/// The `db.operation.name` of one connection's close when the worker stops: the last
/// connection's close checkpoints and removes the write-ahead log.
const CONNECTION_CLOSE: &str = "close";

/// The `db.operation.name` of `operation`: the statement of [`NAMED_STATEMENTS`] a raw
/// statement starts with, or the driver's operation name.
fn operation_name(operation: &Operation) -> &'static str {
    if let Operation::RawSql(raw) = operation
        && let Some(statement) = NAMED_STATEMENTS
            .iter()
            .find(|statement| raw.sql.starts_with(**statement))
    {
        return statement;
    }
    operation.name()
}
/// `sqlite.queue.length`: commands sent to the worker and not yet received, read when the
/// meter collects.
static QUEUE_LENGTH: rift_tracing::ObservableUpDownCounter<1> =
    rift_tracing::ObservableUpDownCounter::declare(
        SCOPE,
        "sqlite.queue.length",
        "{command}",
        &["db.namespace"],
    );
/// `sqlite.transaction.duration`: one transaction from its begin's answer to its commit's
/// or rollback's answer, by `sqlite.transaction.result`.
static TRANSACTION_DURATION: Histogram<2> = Histogram::declare(
    SCOPE,
    "sqlite.transaction.duration",
    &["db.namespace", "sqlite.transaction.result"],
);
/// `sqlite.transaction.statement.count`: the statements one transaction ran, its begin,
/// its savepoints, and its end left out.
static TRANSACTION_STATEMENTS: Histogram<1, u64> = Histogram::declare_count(
    SCOPE,
    "sqlite.transaction.statement.count",
    "{statement}",
    &["db.namespace"],
    &rift_tracing::STATEMENT_BOUNDARIES,
);
/// `sqlite.queue.wait.duration`: one driver operation's round trip to the worker less its
/// execution there: the wait to enter the queue, the wait in it, and the reply.
static QUEUE_WAIT: Histogram<2> = Histogram::declare(
    SCOPE,
    "sqlite.queue.wait.duration",
    &["db.namespace", "db.operation.name"],
);
/// `sqlite.queue.timeouts`: requests the full queue refused once the busy timeout passed.
static QUEUE_TIMEOUTS: Counter<1> = Counter::declare(
    SCOPE,
    "sqlite.queue.timeouts",
    "{timeout}",
    &["db.namespace"],
);
/// `sqlite.write_lock.wait.duration`: one `BEGIN IMMEDIATE` on the worker, the wait for
/// another connection's write lock inside `SQLite` included.
static WRITE_LOCK_WAIT: Histogram<2> = Histogram::declare(
    SCOPE,
    "sqlite.write_lock.wait.duration",
    &["db.namespace", "error.type"],
);
/// `sqlite.transaction.active`: transactions begun and not yet ended, per database, read
/// when the meter collects.
static TRANSACTION_ACTIVE: rift_tracing::ObservableUpDownCounter<1> =
    rift_tracing::ObservableUpDownCounter::declare(
        SCOPE,
        "sqlite.transaction.active",
        "{transaction}",
        &["db.namespace"],
    );
/// `sqlite.connection.refusals`: connection requests the worker refused because it held
/// its bound of connections.
static CONNECTION_REFUSALS: Counter<1> = Counter::declare(
    SCOPE,
    "sqlite.connection.refusals",
    "{connection}",
    &["db.namespace"],
);
/// `sqlite.connection.reaped`: connections the worker removed because their driver
/// connection dropped and no close command removed them first.
static CONNECTION_REAPED: Counter<1> = Counter::declare(
    SCOPE,
    "sqlite.connection.reaped",
    "{connection}",
    &["db.namespace"],
);
/// `sqlite.commit.duration`: one `COMMIT` on the worker, a checkpoint it runs included.
static COMMIT_DURATION: Histogram<1> =
    Histogram::declare(SCOPE, "sqlite.commit.duration", &["db.namespace"]);

/// The `error.type` of a failed driver operation; its `SQLite` result code, when it has
/// one, is the `db.response.status_code` beside it.
const OPERATION_FAILED: &str = "_OTHER";

/// The primary `SQLite` result codes as `db.response.status_code` text, indexed by code.
const RESULT_CODES: [&str; 29] = [
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16",
    "17", "18", "19", "20", "21", "22", "23", "24", "25", "26", "27", "28",
];

/// The `db.response.status_code` of `failure`: the primary result code of the `SQLite`
/// failure in its source chain, or no value when none carries one.
fn result_code(failure: &toasty_core::Error) -> &'static str {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(failure);
    while let Some(current) = source {
        if let Some(rusqlite::Error::SqliteFailure(code, _)) =
            current.downcast_ref::<rusqlite::Error>()
        {
            return usize::try_from(code.extended_code & 0xff)
                .ok()
                .and_then(|primary| RESULT_CODES.get(primary))
                .copied()
                .unwrap_or(OPERATION_FAILED);
        }
        source = current.source();
    }
    ""
}

pub(crate) struct DatabaseThread {
    name: DatabaseName,
    sender: mpsc::Sender<Command>,
    join: Mutex<JoinState>,
    queue_timeout: Duration,
    /// Whether the last request refused at the queue: the first refusal of a run of them
    /// publishes the table of operations in flight, and the next accepted request ends
    /// the run.
    queue_refused: AtomicBool,
    /// Transactions begun on the database's connections and not yet ended.
    transactions_active: AtomicU64,
}

enum JoinState {
    Thread(Option<JoinHandle<()>>),
    Task(tokio::task::JoinHandle<Result<(), String>>),
    Complete(Result<(), String>),
}

/// Why a worker's stop did not end with the worker joined.
#[derive(Debug)]
pub(crate) enum ShutdownFailure {
    /// The deadline passed while the worker still ran: in the queue, before its reply, or
    /// before the thread ended.
    Deadline(toasty_core::Error),
    /// The worker stopped with an error, panicked, or stopped before it answered.
    Failed(toasty_core::Error),
}

impl ShutdownFailure {
    /// The driver error either kind carries.
    pub(crate) fn into_error(self) -> toasty_core::Error {
        match self {
            Self::Deadline(error) | Self::Failed(error) => error,
        }
    }
}

impl std::fmt::Debug for DatabaseThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseThread")
            .field("queue_capacity", &self.sender.max_capacity())
            .field("queue_timeout", &self.queue_timeout)
            .finish_non_exhaustive()
    }
}

impl DatabaseThread {
    /// Starts the worker thread of the database `name`, named for it, which keeps `owner`
    /// from thread entry until the thread exits.
    pub(crate) async fn spawn(
        name: DatabaseName,
        driver: Arc<Sqlite>,
        connections_max: usize,
        queue_timeout: Duration,
        owner: Option<Arc<dyn Send + Sync>>,
    ) -> std::io::Result<Arc<Self>> {
        let capacity = connections_max.max(1);
        let (sender, receiver) = mpsc::channel(capacity);
        let (ready_tx, ready_rx) = oneshot::channel();
        let startup_deadline = Instant::now() + queue_timeout;
        let join = thread::Builder::new()
            .name(name.thread_name().to_owned())
            .spawn(move || {
                let _owner = owner;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => {
                        if ready_tx.send(Ok(())).is_ok() {
                            runtime.block_on(run_database_thread(name, driver, receiver, capacity));
                        }
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })?;
        let ready = timeout_at(startup_deadline, ready_rx).await;
        let startup_error = match ready {
            Ok(Ok(Ok(()))) => None,
            Ok(Ok(Err(error))) => Some(error),
            Ok(Err(error)) => Some(std::io::Error::other(format!(
                "SQLite worker startup failed: {error}"
            ))),
            Err(_) => Some(std::io::Error::other(
                "SQLite worker startup exceeded configured busy timeout",
            )),
        };
        if let Some(error) = startup_error {
            drop(sender);
            let joiner = tokio::task::spawn_blocking(move || join.join().map_err(panic_message));
            match timeout_at(startup_deadline, joiner).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(detail))) => {
                    return Err(std::io::Error::other(format!(
                        "SQLite worker startup cleanup failed: {detail}"
                    )));
                }
                Ok(Err(error)) => {
                    return Err(std::io::Error::other(format!(
                        "SQLite worker startup cleanup task failed: {error}"
                    )));
                }
                Err(_) => return Err(error),
            }
            return Err(error);
        }
        Ok(Arc::new(Self {
            name,
            sender,
            join: Mutex::new(JoinState::Thread(Some(join))),
            queue_timeout,
            queue_refused: AtomicBool::new(false),
            transactions_active: AtomicU64::new(0),
        }))
    }

    /// Holds the worker inside one command until the returned sender fires or drops; the
    /// receiver answers once the worker holds.
    #[cfg(test)]
    pub(crate) async fn hold_for_test(
        &self,
    ) -> Result<(oneshot::Receiver<()>, oneshot::Sender<()>), toasty_core::Error> {
        let (started, started_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        self.sender
            .send(Command::Hold {
                started,
                release: release_rx,
            })
            .await
            .map_err(|_| worker_error("SQLite worker stopped before accepting the hold"))?;
        Ok((started_rx, release))
    }

    #[cfg(test)]
    pub(crate) async fn hold_next_commit_for_test(
        &self,
    ) -> Result<(oneshot::Receiver<()>, oneshot::Sender<()>), toasty_core::Error> {
        let (armed, armed_rx) = oneshot::channel();
        let (started, started_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        self.sender
            .send(Command::HoldNextCommit {
                armed,
                started,
                release: release_rx,
            })
            .await
            .map_err(|_| worker_error("SQLite worker stopped before accepting commit hold"))?;
        armed_rx
            .await
            .map_err(|_| worker_error("SQLite worker stopped before arming commit hold"))?;
        Ok((started_rx, release))
    }

    async fn request<Output>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<Output, toasty_core::Error>>) -> Command,
    ) -> Result<Output, toasty_core::Error>
    where
        Output: Send + 'static,
    {
        let (reply, response) = oneshot::channel();
        let command = command(reply);
        let deadline = Instant::now() + self.queue_timeout;
        let Ok(sent) = timeout_at(deadline, self.sender.send(command)).await else {
            QUEUE_TIMEOUTS.labeled([self.name.label()]).add(1);
            if !self.queue_refused.swap(true, Ordering::Relaxed) {
                rift_tracing::publish_in_flight("SQLite worker queue wait");
            }
            return Err(worker_error(
                "SQLite worker queue wait exceeded configured busy timeout",
            ));
        };
        if self.queue_refused.load(Ordering::Relaxed) {
            self.queue_refused.store(false, Ordering::Relaxed);
        }
        sent.map_err(|_| worker_error("SQLite worker stopped before accepting operation"))?;
        response
            .await
            .map_err(|_| worker_error("SQLite worker stopped before returning operation"))?
    }

    /// Reports `sqlite.queue.length`, the commands sent to the worker and not yet received,
    /// each time the meter collects, until the returned guard drops. The read holds the
    /// queue weakly, so it keeps no worker running, and reports nothing once the queue
    /// closed.
    pub(crate) fn observe_queue_length(&self) -> Option<rift_tracing::ObservationGuard> {
        let name = self.name;
        let queue = self.sender.downgrade();
        QUEUE_LENGTH.observe(move |observation| {
            if let Some(sender) = queue.upgrade() {
                let queued = sender.max_capacity().saturating_sub(sender.capacity());
                observation.observe([name.label()], u64::try_from(queued).unwrap_or(u64::MAX));
            }
        })
    }

    /// Reports `sqlite.transaction.active`, the transactions begun on the database's
    /// connections and not yet ended, each time the meter collects, until the returned guard
    /// drops. The read holds the worker weakly, so it keeps no worker running, and reports
    /// nothing once the worker dropped.
    pub(crate) fn observe_transactions_active(
        self: &Arc<Self>,
    ) -> Option<rift_tracing::ObservationGuard> {
        let name = self.name;
        let worker = Arc::downgrade(self);
        TRANSACTION_ACTIVE.observe(move |observation| {
            if let Some(worker) = worker.upgrade() {
                let active = worker.transactions_active.load(Ordering::Relaxed);
                observation.observe([name.label()], active);
            }
        })
    }

    /// Counts one transaction begun, or with `begun` false one ended.
    fn count_transaction(&self, begun: bool) {
        if begun {
            self.transactions_active.fetch_add(1, Ordering::Relaxed);
        } else {
            // An end without a begin leaves the count at zero, never wrapped.
            let _ = self.transactions_active.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |active| active.checked_sub(1),
            );
        }
    }

    /// [`Self::stop`] with either failure as its driver error.
    #[cfg(test)]
    pub(crate) async fn shutdown(&self, deadline: Instant) -> Result<(), toasty_core::Error> {
        self.stop(deadline)
            .await
            .map_err(ShutdownFailure::into_error)
    }

    /// Stops the worker and joins it by `deadline`, telling a deadline the worker outlasted
    /// apart from a worker that stopped with an error.
    pub(crate) async fn stop(&self, deadline: Instant) -> Result<(), ShutdownFailure> {
        let outlasted = |message: &str| ShutdownFailure::Deadline(worker_error(message));
        let failed = |message: &str| ShutdownFailure::Failed(worker_error(message));
        let mut join_state = timeout_at(deadline, self.join.lock())
            .await
            .map_err(|_| outlasted("SQLite worker shutdown wait exceeded deadline"))?;
        if let JoinState::Complete(result) = &*join_state {
            return result
                .clone()
                .map_err(|error| failed(&format!("SQLite worker stopped with error: {error}")));
        }

        let already_joining = matches!(&*join_state, JoinState::Task(_));
        let response_outcome = if already_joining {
            Ok(())
        } else {
            let (reply, response) = oneshot::channel();
            match timeout_at(deadline, self.sender.reserve()).await {
                Ok(Ok(permit)) => {
                    permit.send(Command::Shutdown { reply });
                    start_join(&mut join_state);
                    match timeout_at(deadline, response).await {
                        Err(_) => Err(outlasted("SQLite worker shutdown exceeded deadline")),
                        Ok(Err(_)) => {
                            Err(failed("SQLite worker stopped before shutdown completed"))
                        }
                        Ok(Ok(reply)) => reply.map_err(ShutdownFailure::Failed),
                    }
                }
                Ok(Err(_)) => {
                    start_join(&mut join_state);
                    Err(failed("SQLite worker stopped before shutdown"))
                }
                Err(_) => {
                    return Err(outlasted(
                        "SQLite worker shutdown queue wait exceeded deadline",
                    ));
                }
            }
        };
        let joined = match &mut *join_state {
            JoinState::Task(join) => match timeout_at(deadline, join).await {
                Ok(Ok(result)) => Some(result.clone()),
                Ok(Err(error)) => Some(Err(error.to_string())),
                Err(_) => None,
            },
            JoinState::Thread(_) => None,
            JoinState::Complete(result) => Some(result.clone()),
        };
        if let Some(result) = joined {
            *join_state = JoinState::Complete(result.clone());
            result
                .map_err(|error| failed(&format!("SQLite worker stopped with error: {error}")))?;
        } else {
            return Err(outlasted("SQLite worker join exceeded deadline"));
        }
        response_outcome
    }
}

#[derive(Debug)]
pub(crate) struct SqliteThreadDriver {
    driver: Arc<Sqlite>,
    actor: Arc<DatabaseThread>,
}

impl SqliteThreadDriver {
    pub(crate) fn new(driver: Arc<Sqlite>, actor: Arc<DatabaseThread>) -> Self {
        Self { driver, actor }
    }
}

#[async_trait]
impl Driver for SqliteThreadDriver {
    fn url(&self) -> std::borrow::Cow<'_, str> {
        self.driver.url()
    }

    fn capability(&self) -> &'static Capability {
        self.driver.capability()
    }

    async fn connect(
        &self,
        context: &ConnectContext,
    ) -> Result<Box<dyn DriverConnection>, toasty_core::Error> {
        let context = context.clone();
        let (id, lease) = self
            .actor
            .request(|reply| Command::Open { context, reply })
            .await?;
        Ok(Box::new(SqliteThreadConnection {
            actor: Arc::clone(&self.actor),
            id,
            _lease: lease,
            transaction_open: false,
            transaction: None,
        }))
    }

    fn max_connections(&self) -> Option<usize> {
        self.driver.max_connections()
    }

    fn generate_migration(&self, schema_diff: &diff::Schema<'_>) -> Migration {
        self.driver.generate_migration(schema_diff)
    }

    async fn reset_db(&self) -> Result<(), toasty_core::Error> {
        self.actor.request(|reply| Command::Reset { reply }).await
    }
}

#[derive(Debug)]
struct SqliteThreadConnection {
    actor: Arc<DatabaseThread>,
    id: u64,
    _lease: Arc<()>,
    /// Whether a transaction began on the connection and has not ended.
    transaction_open: bool,
    /// The open transaction's measurements, absent outside one.
    transaction: Option<OpenTransaction>,
}

/// What one open transaction measures until it ends: when its begin was answered and the
/// statements it ran since.
#[derive(Clone, Copy, Debug)]
struct OpenTransaction {
    begun: Instant,
    statements: u64,
}

impl Drop for SqliteThreadConnection {
    fn drop(&mut self) {
        if std::mem::take(&mut self.transaction_open) {
            self.actor.count_transaction(false);
        }
        // Closing a connection inside a transaction rolls it back.
        self.end_transaction(TRANSACTION_ROLLBACK);
        let _ = self.actor.sender.try_send(Command::Close { id: self.id });
    }
}

/// What one driver operation is to the measurements: its name, and its place in a
/// transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OperationRole {
    /// `BEGIN IMMEDIATE`: takes the write lock.
    BeginImmediate,
    /// Any other `BEGIN`.
    Begin,
    Commit,
    Rollback,
    /// A savepoint inside a transaction: set, released, or rolled back to.
    Savepoint,
    /// A statement.
    Statement,
}

impl OperationRole {
    fn of(operation: &Operation) -> Self {
        match operation {
            Operation::Transaction(Transaction::Start {
                mode: TransactionMode::Immediate,
                ..
            }) => Self::BeginImmediate,
            Operation::Transaction(Transaction::Start { .. }) => Self::Begin,
            Operation::Transaction(Transaction::Commit) => Self::Commit,
            Operation::Transaction(Transaction::Rollback) => Self::Rollback,
            Operation::Transaction(_) => Self::Savepoint,
            _ => Self::Statement,
        }
    }
}

/// What the worker answers a driver operation with: the driver's result and how long the
/// worker spent executing it, absent when the clock regressed.
#[derive(Debug)]
struct Executed {
    result: Result<ExecResponse, toasty_core::Error>,
    took: Option<Duration>,
}

impl SqliteThreadConnection {
    /// Records one operation the worker executed: its duration, its queue wait, and the
    /// transaction it began or ended. `round_trip` spans the request from its send to its
    /// reply.
    fn record(
        &mut self,
        operation: &'static str,
        role: OperationRole,
        executed: &Executed,
        round_trip: Option<PerformanceMeasurement>,
    ) {
        let database = self.actor.name.label();
        let (status, failed) = match &executed.result {
            Ok(_) => ("", ""),
            Err(failure) => (result_code(failure), OPERATION_FAILED),
        };
        if let Some(took) = executed.took {
            OPERATION_DURATION
                .labeled([DB_SYSTEM, database, operation, status, failed])
                .record(took);
            if let Some(round_trip) = round_trip {
                QUEUE_WAIT
                    .labeled([database, operation])
                    .record(round_trip.elapsed().saturating_sub(took));
            }
            if role == OperationRole::BeginImmediate {
                WRITE_LOCK_WAIT.labeled([database, failed]).record(took);
            }
            if role == OperationRole::Commit && executed.result.is_ok() {
                COMMIT_DURATION.labeled([database]).record(took);
            }
        }
        let ended = match role {
            OperationRole::BeginImmediate | OperationRole::Begin => {
                if executed.result.is_ok() && !self.transaction_open {
                    self.transaction_open = true;
                    self.actor.count_transaction(true);
                    self.transaction = Some(OpenTransaction {
                        begun: Instant::now(),
                        statements: 0,
                    });
                }
                None
            }
            // A refused commit leaves the transaction open for its rollback.
            OperationRole::Commit => executed.result.is_ok().then_some(TRANSACTION_COMMIT),
            OperationRole::Rollback => Some(TRANSACTION_ROLLBACK),
            OperationRole::Savepoint => None,
            OperationRole::Statement => {
                if let Some(open) = &mut self.transaction {
                    open.statements = open.statements.saturating_add(1);
                }
                None
            }
        };
        if let Some(result) = ended {
            if std::mem::take(&mut self.transaction_open) {
                self.actor.count_transaction(false);
            }
            self.end_transaction(result);
        }
    }

    /// Records the open transaction's duration under `result` and its statement count,
    /// and forgets it; outside a transaction it records nothing.
    fn end_transaction(&mut self, result: &'static str) {
        let Some(open) = self.transaction.take() else {
            return;
        };
        let database = self.actor.name.label();
        TRANSACTION_DURATION
            .labeled([database, result])
            .record(open.begun.elapsed());
        TRANSACTION_STATEMENTS
            .labeled([database])
            .record(open.statements);
    }
}

/// The `sqlite.transaction.result` of a committed transaction.
const TRANSACTION_COMMIT: &str = "commit";
/// The `sqlite.transaction.result` of a transaction rolled back, by request or by closing
/// its connection.
const TRANSACTION_ROLLBACK: &str = "rollback";

#[async_trait]
impl DriverConnection for SqliteThreadConnection {
    async fn exec(
        &mut self,
        schema: &Arc<Schema>,
        operation: Operation,
    ) -> Result<ExecResponse, toasty_core::Error> {
        let id = self.id;
        let schema = Arc::clone(schema);
        let name = operation_name(&operation);
        let role = OperationRole::of(&operation);
        let answered;
        let round_trip = rift_tracing::measure_elapsed!("sqlite.queue", {
            answered = self
                .actor
                .request(|reply| Command::Exec {
                    id,
                    schema,
                    operation: Box::new(operation),
                    reply,
                })
                .await;
        })
        .ok()
        .map(|((), round_trip)| round_trip);
        let executed = answered?;
        self.record(name, role, &executed, round_trip);
        executed.result
    }

    async fn push_schema(&mut self, schema: &Schema) -> Result<(), toasty_core::Error> {
        let id = self.id;
        let schema = Arc::new(Schema {
            app: toasty_core::schema::app::Schema::default(),
            db: schema.db.clone(),
            mapping: toasty_core::schema::mapping::Mapping {
                models: std::iter::empty().collect(),
                document_columns: std::iter::empty().collect(),
            },
        });
        self.actor
            .request(|reply| Command::PushSchema { id, schema, reply })
            .await
    }

    async fn applied_migrations(&mut self) -> Result<Vec<AppliedMigration>, toasty_core::Error> {
        let id = self.id;
        self.actor
            .request(|reply| Command::AppliedMigrations { id, reply })
            .await
    }

    async fn apply_migration(
        &mut self,
        id: u64,
        name: &str,
        migration: &Migration,
    ) -> Result<(), toasty_core::Error> {
        let connection_id = self.id;
        let name = name.to_owned();
        let statements = migration
            .statements()
            .into_iter()
            .map(str::to_owned)
            .collect();
        self.actor
            .request(|reply| Command::ApplyMigration {
                connection_id,
                id,
                name,
                statements,
                reply,
            })
            .await
    }
}

enum Command {
    Open {
        context: ConnectContext,
        reply: oneshot::Sender<Result<(u64, Arc<()>), toasty_core::Error>>,
    },
    Exec {
        id: u64,
        schema: Arc<Schema>,
        operation: Box<Operation>,
        reply: oneshot::Sender<Result<Executed, toasty_core::Error>>,
    },
    PushSchema {
        id: u64,
        schema: Arc<Schema>,
        reply: oneshot::Sender<Result<(), toasty_core::Error>>,
    },
    AppliedMigrations {
        id: u64,
        reply: oneshot::Sender<Result<Vec<AppliedMigration>, toasty_core::Error>>,
    },
    ApplyMigration {
        connection_id: u64,
        id: u64,
        name: String,
        statements: Vec<String>,
        reply: oneshot::Sender<Result<(), toasty_core::Error>>,
    },
    Reset {
        reply: oneshot::Sender<Result<(), toasty_core::Error>>,
    },
    Close {
        id: u64,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), toasty_core::Error>>,
    },
    #[cfg(test)]
    Panic,
    #[cfg(test)]
    Hold {
        started: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
    #[cfg(test)]
    HoldNextCommit {
        armed: oneshot::Sender<()>,
        started: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    },
    #[cfg(test)]
    Noop(oneshot::Sender<()>),
    #[cfg(test)]
    ThreadName(oneshot::Sender<Option<String>>),
}

struct OwnedConnection {
    connection: Box<dyn DriverConnection>,
    lease: Weak<()>,
}

async fn run_database_thread(
    name: DatabaseName,
    driver: Arc<Sqlite>,
    receiver: mpsc::Receiver<Command>,
    connections_max: usize,
) {
    DatabaseWorker::new(name, driver, connections_max)
        .run(receiver)
        .await;
}

struct DatabaseWorker {
    name: DatabaseName,
    driver: Arc<Sqlite>,
    connections: HashMap<u64, OwnedConnection>,
    next_id: u64,
    connections_max: usize,
    #[cfg(test)]
    commit_hold: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
}

impl DatabaseWorker {
    fn new(name: DatabaseName, driver: Arc<Sqlite>, connections_max: usize) -> Self {
        Self {
            name,
            driver,
            connections: HashMap::new(),
            next_id: 0,
            connections_max,
            #[cfg(test)]
            commit_hold: None,
        }
    }

    async fn run(mut self, mut receiver: mpsc::Receiver<Command>) {
        let mut reap = interval(CONNECTION_REAP_SPAN);
        loop {
            tokio::select! {
                _ = reap.tick() => self.reap_connections(),
                command = receiver.recv() => {
                    let Some(command) = command else { break; };
                    if !self.handle(command).await {
                        break;
                    }
                    self.reap_connections();
                }
            }
        }
    }

    async fn handle(&mut self, command: Command) -> bool {
        match command {
            Command::Open { context, reply } => {
                self.open(context, reply).await;
            }
            Command::Exec {
                id,
                schema,
                operation,
                reply,
            } => {
                self.exec(id, schema, operation, reply).await;
            }
            Command::PushSchema { id, schema, reply } => {
                self.push_schema(id, schema, reply).await;
            }
            Command::AppliedMigrations { id, reply } => {
                self.applied_migrations(id, reply).await;
            }
            Command::ApplyMigration {
                connection_id,
                id,
                name,
                statements,
                reply,
            } => {
                self.apply_migration(connection_id, id, name, statements, reply)
                    .await;
            }
            Command::Reset { reply } => self.reset(reply).await,
            Command::Close { id } => {
                if self
                    .connections
                    .get(&id)
                    .is_some_and(|owned| owned.lease.strong_count() == 0)
                {
                    self.connections.remove(&id);
                }
            }
            Command::Shutdown { reply } => {
                self.close_connections();
                let _ = reply.send(Ok(()));
                return false;
            }
            #[cfg(test)]
            Command::Panic => panic!("test SQLite worker panic"),
            #[cfg(test)]
            Command::Hold { started, release } => {
                let _ = started.send(());
                let _ = release.await;
            }
            #[cfg(test)]
            Command::HoldNextCommit {
                armed,
                started,
                release,
            } => {
                assert!(
                    self.commit_hold.is_none(),
                    "only one commit hold may be armed"
                );
                self.commit_hold = Some((started, release));
                let _ = armed.send(());
            }
            #[cfg(test)]
            Command::Noop(reply) => {
                let _ = reply.send(());
            }
            #[cfg(test)]
            Command::ThreadName(reply) => {
                let _ = reply.send(thread::current().name().map(str::to_owned));
            }
        }
        true
    }

    /// Closes every connection the worker holds, recording each close as one
    /// `db.client.operation.duration` point named [`CONNECTION_CLOSE`]. The driver closes a
    /// connection when it drops and reports no failure of that close.
    fn close_connections(&mut self) {
        let database = self.name.label();
        for (_, owned) in self.connections.drain() {
            let closed = rift_tracing::measure_elapsed!("db.client.operation", drop(owned));
            if let Ok(((), took)) = closed {
                OPERATION_DURATION
                    .labeled([DB_SYSTEM, database, CONNECTION_CLOSE, "", ""])
                    .record(took.elapsed());
            }
        }
    }

    async fn open(
        &mut self,
        context: ConnectContext,
        reply: oneshot::Sender<Result<(u64, Arc<()>), toasty_core::Error>>,
    ) {
        self.reap_connections();
        let result = if self.connections.len() >= self.connections_max {
            CONNECTION_REFUSALS.labeled([self.name.label()]).add(1);
            Err(worker_error("SQLite connection bound reached"))
        } else {
            match self.driver.connect(&context).await {
                Ok(connection) => match self.next_id.checked_add(1) {
                    Some(id) => {
                        self.next_id = id;
                        let lease = Arc::new(());
                        self.connections.insert(
                            id,
                            OwnedConnection {
                                connection,
                                lease: Arc::downgrade(&lease),
                            },
                        );
                        Ok((id, lease))
                    }
                    None => Err(worker_error("SQLite connection id exhausted")),
                },
                Err(error) => Err(error),
            }
        };
        let _ = reply.send(result);
    }

    async fn exec(
        &mut self,
        id: u64,
        schema: Arc<Schema>,
        operation: Box<Operation>,
        reply: oneshot::Sender<Result<Executed, toasty_core::Error>>,
    ) {
        let operation = *operation;
        #[cfg(test)]
        if operation.is_transaction_commit()
            && let Some((started, release)) = self.commit_hold.take()
        {
            let _ = started.send(());
            let _ = release.await;
        }
        let result;
        let took = rift_tracing::measure_elapsed!("db.client.operation", {
            result = match self.connections.get_mut(&id) {
                Some(owned) => owned.connection.exec(&schema, operation).await,
                None => Err(worker_error("SQLite connection is closed")),
            };
        })
        .ok()
        .map(|((), took)| took.elapsed());
        let _ = reply.send(Ok(Executed { result, took }));
    }

    async fn push_schema(
        &mut self,
        id: u64,
        schema: Arc<Schema>,
        reply: oneshot::Sender<Result<(), toasty_core::Error>>,
    ) {
        let result = match self.connections.get_mut(&id) {
            Some(owned) => owned.connection.push_schema(&schema).await,
            None => Err(worker_error("SQLite connection is closed")),
        };
        let _ = reply.send(result);
    }

    async fn applied_migrations(
        &mut self,
        id: u64,
        reply: oneshot::Sender<Result<Vec<AppliedMigration>, toasty_core::Error>>,
    ) {
        let result = match self.connections.get_mut(&id) {
            Some(owned) => owned.connection.applied_migrations().await,
            None => Err(worker_error("SQLite connection is closed")),
        };
        let _ = reply.send(result);
    }

    async fn apply_migration(
        &mut self,
        connection_id: u64,
        id: u64,
        name: String,
        statements: Vec<String>,
        reply: oneshot::Sender<Result<(), toasty_core::Error>>,
    ) {
        let migration = Migration::new_sql_with_breakpoints(&statements);
        let result = match self.connections.get_mut(&connection_id) {
            Some(owned) => {
                owned
                    .connection
                    .apply_migration(id, &name, &migration)
                    .await
            }
            None => Err(worker_error("SQLite connection is closed")),
        };
        let _ = reply.send(result);
    }

    /// Removes every connection whose driver connection dropped, and counts each one in
    /// `sqlite.connection.reaped`.
    fn reap_connections(&mut self) {
        let held = self.connections.len();
        self.connections
            .retain(|_, connection| connection.lease.strong_count() > 0);
        let reaped = held.saturating_sub(self.connections.len());
        if reaped > 0 {
            CONNECTION_REAPED
                .labeled([self.name.label()])
                .add(u64::try_from(reaped).unwrap_or(u64::MAX));
        }
    }

    async fn reset(&self, reply: oneshot::Sender<Result<(), toasty_core::Error>>) {
        let result = if self.connections.is_empty() {
            self.driver.reset_db().await
        } else {
            Err(worker_error(
                "SQLite reset refused while connections are open",
            ))
        };
        let _ = reply.send(result);
    }
}

fn worker_error(message: &str) -> toasty_core::Error {
    toasty_core::Error::driver_operation_failed(std::io::Error::other(message.to_owned()))
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => match payload.downcast::<&'static str>() {
            Ok(message) => (*message).to_owned(),
            Err(_) => "SQLite worker panicked with a non-string payload".to_owned(),
        },
    }
}

fn start_join(join_state: &mut JoinState) {
    let thread_join = match join_state {
        JoinState::Thread(join) => join.take(),
        JoinState::Task(_) | JoinState::Complete(_) => None,
    };
    if let Some(join) = thread_join {
        *join_state = JoinState::Task(tokio::task::spawn_blocking(move || {
            join.join().map_err(panic_message)
        }));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use toasty_core::driver::{ConnectContext, Driver as _};
    use toasty_driver_sqlite::Sqlite;
    use tokio::sync::oneshot;
    use tokio::time::Instant;

    use super::{Command, DatabaseThread, SqliteThreadDriver};
    use crate::database::DatabaseName;

    async fn driver(
        path: &std::path::Path,
        queue_timeout: Duration,
    ) -> (Arc<DatabaseThread>, SqliteThreadDriver) {
        let sqlite = Arc::new(Sqlite::open(path));
        let actor = DatabaseThread::spawn(
            DatabaseName::Index,
            Arc::clone(&sqlite),
            1,
            queue_timeout,
            None,
        )
        .await
        .expect("SQLite worker must start");
        let driver = SqliteThreadDriver::new(sqlite, Arc::clone(&actor));
        (actor, driver)
    }

    #[tokio::test]
    async fn cancelled_connect_after_open_releases_slot() {
        let directory = tempfile::tempdir().expect("fixture directory must open");
        let (actor, driver) = driver(&directory.path().join("db"), Duration::from_secs(1)).await;
        let (reply, response) = oneshot::channel();
        actor
            .sender
            .send(Command::Open {
                context: ConnectContext::default(),
                reply,
            })
            .await
            .expect("open must queue");
        drop(response);
        let (done, completed) = oneshot::channel();
        actor
            .sender
            .send(Command::Noop(done))
            .await
            .expect("barrier must queue after open");
        completed.await.expect("barrier must complete");

        let connection = driver
            .connect(&ConnectContext::default())
            .await
            .expect("cancelled connect must release its opened slot");
        drop(connection);
        actor
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await
            .expect("worker must stop");
        actor
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await
            .expect("completed shutdown must be idempotent");
    }

    #[tokio::test]
    async fn dropped_connection_reaps_when_close_cannot_queue() {
        let directory = tempfile::tempdir().expect("fixture directory must open");
        let (actor, driver) = driver(&directory.path().join("db"), Duration::from_secs(1)).await;
        let connection = driver
            .connect(&ConnectContext::default())
            .await
            .expect("first connection must open");

        let (started, is_started) = oneshot::channel();
        let (release, released) = oneshot::channel();
        actor
            .sender
            .send(Command::Hold {
                started,
                release: released,
            })
            .await
            .expect("hold command must queue");
        is_started.await.expect("worker must hold queue");
        let (done, completed) = oneshot::channel();
        actor
            .sender
            .try_send(Command::Noop(done))
            .expect("one queued command must fill bounded queue");
        drop(connection);
        release.send(()).expect("worker must resume");
        completed.await.expect("queued command must complete");

        let connection = tokio::time::timeout(
            Duration::from_secs(1),
            driver.connect(&ConnectContext::default()),
        )
        .await
        .expect("connection reaping must finish within one second")
        .expect("dropped proxy must not leak actor slot");
        drop(connection);
        actor
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await
            .expect("worker must stop");
    }

    /// The `{connection}` count `name` holds for the index database in `metrics`, absent
    /// when nothing was counted.
    fn connections_counted(
        metrics: &rift_tracing::MetricSnapshot,
        name: &str,
    ) -> Option<rift_tracing::SeriesValue> {
        metrics
            .find(name, &[("db.namespace", "index")])
            .map(|series| {
                assert_eq!(series.unit(), "{connection}");
                series.value().clone()
            })
    }

    /// A connection request past the worker's bound of one connection is refused and counted
    /// once in `sqlite.connection.refusals`.
    #[tokio::test]
    async fn a_connection_past_the_bound_counts_one_refusal() {
        let (recorder, _drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("the recorder installs");
        let directory = tempfile::tempdir().expect("fixture directory must open");
        let (actor, driver) = driver(&directory.path().join("db"), Duration::from_secs(1)).await;
        let held = driver
            .connect(&ConnectContext::default())
            .await
            .expect("the first connection opens");
        let refused = driver
            .connect(&ConnectContext::default())
            .await
            .expect_err("a second connection passes the bound of one");
        assert!(
            refused.to_string().contains("connection bound reached"),
            "{refused}"
        );
        let metrics = recorder.metrics();
        assert_eq!(
            connections_counted(&metrics, "sqlite.connection.refusals"),
            Some(rift_tracing::SeriesValue::Sum(1.0))
        );
        assert_eq!(
            connections_counted(&metrics, "sqlite.connection.reaped"),
            None,
            "the held connection is not reaped"
        );
        drop(held);
        actor
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await
            .expect("worker must stop");
    }

    /// A connection dropped while its close command cannot queue is reaped and counted once
    /// in `sqlite.connection.reaped`.
    #[tokio::test]
    async fn a_connection_dropped_behind_a_full_queue_counts_one_reap() {
        let (recorder, _drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("the recorder installs");
        let directory = tempfile::tempdir().expect("fixture directory must open");
        let (actor, driver) = driver(&directory.path().join("db"), Duration::from_secs(1)).await;
        let connection = driver
            .connect(&ConnectContext::default())
            .await
            .expect("first connection must open");
        let (started, release) = actor
            .hold_for_test()
            .await
            .expect("hold command must queue");
        started.await.expect("worker must hold queue");
        let (done, completed) = oneshot::channel();
        actor
            .sender
            .try_send(Command::Noop(done))
            .expect("one queued command must fill bounded queue");
        drop(connection);
        release.send(()).expect("worker must resume");
        completed.await.expect("queued command must complete");
        let reopened = driver
            .connect(&ConnectContext::default())
            .await
            .expect("the reaped slot opens again");

        let metrics = recorder.metrics();
        assert_eq!(
            connections_counted(&metrics, "sqlite.connection.reaped"),
            Some(rift_tracing::SeriesValue::Sum(1.0))
        );
        assert_eq!(
            connections_counted(&metrics, "sqlite.connection.refusals"),
            None,
            "the reaped slot refused nothing"
        );
        drop(reopened);
        actor
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await
            .expect("worker must stop");
    }

    #[tokio::test]
    async fn full_queue_and_worker_panic_keep_error_sources() {
        let directory = tempfile::tempdir().expect("fixture directory must open");
        let (actor, driver) = driver(&directory.path().join("db"), Duration::from_millis(20)).await;
        let (started, is_started) = oneshot::channel();
        let (release, released) = oneshot::channel();
        actor
            .sender
            .send(Command::Hold {
                started,
                release: released,
            })
            .await
            .expect("hold command must queue");
        is_started.await.expect("worker must hold queue");
        let (done, completed) = oneshot::channel();
        actor
            .sender
            .try_send(Command::Noop(done))
            .expect("one queued command must fill bounded queue");
        let error = driver
            .connect(&ConnectContext::default())
            .await
            .expect_err("full queue must refuse after configured wait");
        assert!(std::error::Error::source(&error).is_some());

        release.send(()).expect("worker must resume");
        completed.await.expect("queued command must complete");
        actor
            .sender
            .send(Command::Panic)
            .await
            .expect("panic command must queue");
        let error = driver
            .connect(&ConnectContext::default())
            .await
            .expect_err("worker panic must close pending driver requests");
        assert!(std::error::Error::source(&error).is_some());
        let error = actor
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await
            .expect_err("shutdown must report worker panic");
        assert!(std::error::Error::source(&error).is_some());
        assert!(
            error.to_string().contains("test SQLite worker panic"),
            "shutdown error must preserve worker panic payload: {error}"
        );
    }

    /// A held worker that outlasts the stop's deadline answers a deadline failure, and a
    /// worker that panicked answers a failure of its own whatever the deadline.
    #[tokio::test]
    async fn a_stop_tells_a_worker_past_its_deadline_from_a_failed_one() {
        let directory = tempfile::tempdir().expect("fixture directory must open");
        let (held, _held_driver) =
            driver(&directory.path().join("held"), Duration::from_secs(1)).await;
        let (holding, release) = held.hold_for_test().await.expect("hold command must queue");
        holding.await.expect("worker must hold");

        let outlasted = held.stop(Instant::now() + Duration::from_millis(10)).await;

        assert!(
            matches!(outlasted, Err(super::ShutdownFailure::Deadline(_))),
            "a held worker outlasts the deadline: {outlasted:?}"
        );
        release.send(()).expect("worker must resume");
        held.stop(Instant::now() + Duration::from_secs(1))
            .await
            .expect("a released worker stops");

        let (panicked, _panicked_driver) =
            driver(&directory.path().join("panicked"), Duration::from_secs(1)).await;
        panicked
            .sender
            .send(Command::Panic)
            .await
            .expect("panic command must queue");

        let failed = panicked.stop(Instant::now() + Duration::from_secs(1)).await;

        assert!(
            matches!(failed, Err(super::ShutdownFailure::Failed(_))),
            "a panicked worker fails its stop: {failed:?}"
        );
    }

    #[tokio::test]
    async fn a_run_of_queue_refusals_publishes_the_operations_in_flight_once() {
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("the recorder installs");
        let directory = tempfile::tempdir().expect("fixture directory must open");
        let (actor, driver) = driver(&directory.path().join("db"), Duration::from_millis(20)).await;
        let (started, is_started) = oneshot::channel();
        let (release, released) = oneshot::channel();
        actor
            .sender
            .send(Command::Hold {
                started,
                release: released,
            })
            .await
            .expect("hold command must queue");
        is_started.await.expect("worker must hold queue");
        let (done, completed) = oneshot::channel();
        actor
            .sender
            .try_send(Command::Noop(done))
            .expect("one queued command must fill bounded queue");
        let refusals: u8 = 3;
        for _ in 0..refusals {
            rift_tracing::traced!(component = "lexical", operation = "lexical.commit", async {
                driver.connect(&ConnectContext::default()).await
            })
            .await
            .expect_err("full queue must refuse after configured wait");
        }
        release.send(()).expect("worker must resume");
        completed.await.expect("queued command must complete");
        drop(
            driver
                .connect(&ConnectContext::default())
                .await
                .expect("a drained queue accepts the next request"),
        );
        let metrics = recorder.metrics();
        drop(recorder);

        let tables = drain
            .queued_records()
            .into_iter()
            .filter(|record| record.message() == "operations in flight")
            .collect::<Vec<_>>();
        assert_eq!(tables.len(), 1, "one table per run of refusals");
        let fields: serde_json::Value =
            serde_json::from_str(tables[0].fields()).expect("table fields are JSON");
        assert_eq!(fields["reason"], "SQLite worker queue wait");
        assert!(
            fields["operations"]
                .as_str()
                .is_some_and(|listed| listed.contains("lexical.commit")),
            "the refused operation is in flight: {fields}"
        );
        assert!(
            !actor.queue_refused.load(super::Ordering::Relaxed),
            "an accepted request ends the run"
        );
        let timeouts = metrics
            .find("sqlite.queue.timeouts", &[("db.namespace", "index")])
            .expect("the refusals were counted");
        assert_eq!(timeouts.unit(), "{timeout}");
        assert_eq!(
            timeouts.value(),
            &rift_tracing::SeriesValue::Sum(f64::from(refusals))
        );
        actor
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await
            .expect("worker must stop");
    }

    #[tokio::test]
    async fn shutdown_queue_wait_respects_deadline_and_can_be_retried() {
        let directory = tempfile::tempdir().expect("fixture directory must open");
        let (actor, _driver) = driver(&directory.path().join("db"), Duration::from_secs(1)).await;
        let (started, is_started) = oneshot::channel();
        let (release, released) = oneshot::channel();
        actor
            .sender
            .send(Command::Hold {
                started,
                release: released,
            })
            .await
            .expect("hold command must queue");
        is_started.await.expect("worker must hold queue");
        let (done, completed) = oneshot::channel();
        actor
            .sender
            .try_send(Command::Noop(done))
            .expect("one queued command must fill bounded queue");

        let error = actor
            .shutdown(Instant::now() + Duration::from_millis(10))
            .await
            .expect_err("shutdown must honor its queue deadline");
        assert!(error.to_string().contains("queue wait exceeded deadline"));

        release.send(()).expect("worker must resume");
        completed.await.expect("queued command must complete");
        actor
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await
            .expect("shutdown can retry after the queue drains");
    }

    #[tokio::test]
    async fn shutdown_response_timeout_keeps_join_for_retry() {
        let directory = tempfile::tempdir().expect("fixture directory must open");
        let (actor, _driver) = driver(&directory.path().join("db"), Duration::from_secs(1)).await;
        let (started, is_started) = oneshot::channel();
        let (release, released) = oneshot::channel();
        actor
            .sender
            .send(Command::Hold {
                started,
                release: released,
            })
            .await
            .expect("hold command must queue");
        is_started
            .await
            .expect("worker must hold command processing");

        let error = actor
            .shutdown(Instant::now() + Duration::from_millis(10))
            .await
            .expect_err("shutdown response must respect its deadline");
        assert!(error.to_string().contains("deadline"));

        release.send(()).expect("worker must resume");
        actor
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await
            .expect("retry must await previously accepted shutdown and join");
    }

    #[tokio::test]
    async fn timed_out_shutdown_retains_owner_after_caller_drops_the_actor() {
        let directory = tempfile::tempdir().expect("fixture directory must open");
        let lock_path = directory.path().join("owner.lock");
        let owner = Arc::new(std::fs::File::create(&lock_path).expect("owner file opens"));
        owner.lock().expect("owner locks its file");
        let retained = Arc::downgrade(&owner);
        let actor = DatabaseThread::spawn(
            DatabaseName::Index,
            Arc::new(Sqlite::open(directory.path().join("db"))),
            1,
            Duration::from_secs(1),
            Some(Arc::<std::fs::File>::clone(&owner)),
        )
        .await
        .expect("SQLite worker starts");
        drop(owner);
        let (started, is_started) = oneshot::channel();
        let (release, released) = oneshot::channel();
        actor
            .sender
            .send(Command::Hold {
                started,
                release: released,
            })
            .await
            .expect("worker hold queues");
        is_started.await.expect("worker is held");
        actor
            .shutdown(Instant::now() + Duration::from_millis(10))
            .await
            .expect_err("held worker exceeds shutdown deadline");
        drop(actor);
        assert!(retained.upgrade().is_some(), "worker still owns its lease");
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("contender opens same lock");
        assert!(
            contender.try_lock().is_err(),
            "another owner cannot enter while worker runs"
        );
        release.send(()).expect("worker resumes");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if retained.upgrade().is_none() && contender.try_lock().is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("worker releases owner after real exit");
    }

    #[tokio::test]
    async fn cancelled_shutdown_retains_accepted_worker_join() {
        let directory = tempfile::tempdir().expect("fixture directory must open");
        let (actor, _driver) = driver(&directory.path().join("db"), Duration::from_secs(1)).await;
        let (started, is_started) = oneshot::channel();
        let (release, released) = oneshot::channel();
        actor
            .sender
            .send(Command::Hold {
                started,
                release: released,
            })
            .await
            .expect("hold command must queue");
        is_started
            .await
            .expect("worker must hold command processing");

        let shutdown_actor = Arc::clone(&actor);
        let shutdown = tokio::spawn(async move {
            shutdown_actor
                .shutdown(Instant::now() + Duration::from_secs(1))
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while actor.sender.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown command must enter bounded queue");
        shutdown.abort();
        release.send(()).expect("worker must resume");
        actor
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await
            .expect("cancelled shutdown must leave accepted join awaitable");
    }

    /// Each database's worker thread carries the database's own name, short enough that
    /// Linux keeps it whole.
    #[tokio::test]
    async fn each_database_worker_is_named_for_its_database() {
        let directory = tempfile::tempdir().expect("fixture directory must open");
        for (name, expected) in [
            (DatabaseName::Index, "rift-db-index"),
            (DatabaseName::Vectors, "rift-db-vectors"),
        ] {
            let actor = DatabaseThread::spawn(
                name,
                Arc::new(Sqlite::open(name.path(directory.path()))),
                1,
                Duration::from_secs(1),
                None,
            )
            .await
            .expect("SQLite worker starts");
            let (reply, named) = oneshot::channel();
            actor
                .sender
                .send(Command::ThreadName(reply))
                .await
                .expect("name request queues");
            let named = named.await.expect("worker answers its name");
            assert_eq!(named.as_deref(), Some(expected));
            assert!(expected.len() <= 15, "Linux keeps 15 bytes: {expected}");
            actor
                .shutdown(Instant::now() + Duration::from_secs(1))
                .await
                .expect("worker must stop");
        }
    }

    /// A driver failure names the primary result code of the `SQLite` failure it wraps,
    /// an extended code included, and a failure that wraps none names no code.
    #[test]
    fn a_driver_failure_names_its_primary_sqlite_result_code() {
        use rusqlite::ffi;

        let failure = |code| {
            toasty_core::Error::driver_operation_failed(rusqlite::Error::SqliteFailure(
                ffi::Error::new(code),
                None,
            ))
        };
        assert_eq!(super::result_code(&failure(ffi::SQLITE_BUSY)), "5");
        assert_eq!(
            super::result_code(&failure(ffi::SQLITE_LOCKED_SHAREDCACHE)),
            "6"
        );
        assert_eq!(super::result_code(&failure(ffi::SQLITE_IOERR_READ)), "10");
        assert_eq!(super::result_code(&super::worker_error("stopped")), "");
    }
}
