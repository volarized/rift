//! Verifies Rift's required Toasty and `SQLite` behavior against pinned releases.
//!
//! Toasty 0.10 has no typed FTS5 virtual-table or `MATCH` API and no driver-level
//! per-connection setup hook. Raw SQL remains limited to this compatibility boundary:
//! every dedicated Toasty connection receives required pragmas, while FTS rows are
//! parameterized and transacted with typed writes. Remove raw FTS SQL when Toasty
//! supports typed virtual tables, `MATCH`, and rebuild operations on its executor.
//! Toasty model relations also omit `SQLite` foreign-key DDL. Reviewed migrations own
//! those constraints until Toasty-generated migrations emit them.

use std::path::Path;

use tempfile::TempDir;
use toasty::{
    Db, Executor,
    db::Connection,
    migration::{MigrationFile, MigrationReport, MigrationSet},
    stmt::{Type, Value},
};
use toasty_driver_sqlite::Sqlite;

const BUSY_TIMEOUT_MS: i64 = 1_000;
const FTS_QUERY_ROWS_MAX: i64 = 8;
const FTS_REBUILD_ROWS_MAX: usize = 8;
const MIGRATION_FILES: &[MigrationFile] = &[MigrationFile::new(
    1,
    "index_schema",
    "CREATE TABLE source_units (
        id TEXT PRIMARY KEY NOT NULL,
        project_path TEXT NOT NULL,
        revision BIGINT NOT NULL
    )
-- #[toasty::breakpoint]
CREATE UNIQUE INDEX source_units_project_path ON source_units(project_path)
-- #[toasty::breakpoint]
CREATE TABLE symbols (
        id TEXT PRIMARY KEY NOT NULL,
        source_unit_id TEXT NOT NULL,
        name TEXT NOT NULL,
        content TEXT NOT NULL,
        FOREIGN KEY(source_unit_id) REFERENCES source_units(id)
    )
-- #[toasty::breakpoint]
CREATE INDEX symbols_source_unit_id ON symbols(source_unit_id)",
)];
const MIGRATIONS: MigrationSet = MigrationSet::new(MIGRATION_FILES);

#[derive(Debug, toasty::Model)]
#[table = "source_units"]
struct SourceUnitRecord {
    #[key]
    id: String,
    #[unique]
    project_path: String,
    revision: i64,
    #[has_many(pair = source_unit)]
    symbols: toasty::Deferred<Vec<SymbolRecord>>,
}

#[derive(Debug, toasty::Model)]
#[table = "symbols"]
struct SymbolRecord {
    #[key]
    id: String,
    #[index]
    source_unit_id: String,
    #[belongs_to]
    source_unit: toasty::Deferred<SourceUnitRecord>,
    name: String,
    content: String,
}

async fn open_database(path: &Path) -> toasty::Result<(Db, MigrationReport)> {
    let mut builder = Db::builder();
    builder
        .models(toasty::models!(SourceUnitRecord, SymbolRecord))
        .max_pool_size(4);
    let database = builder.build(Sqlite::open(path)).await?;
    let report = MIGRATIONS.apply(&database).await?;
    Ok((database, report))
}

async fn open_configured_connection(database: &Db) -> toasty::Result<Connection> {
    let mut connection = database.connection().await?;
    let journal_mode = toasty::sql::query("PRAGMA journal_mode = WAL")
        .column_types([Type::String])
        .exec(&mut connection)
        .await?;
    require_single_row(
        "journal_mode",
        &journal_mode,
        &[Value::String("wal".to_owned())],
    )?;

    toasty::sql::query("PRAGMA foreign_keys = ON")
        .exec(&mut connection)
        .await?;
    toasty::sql::query(format!("PRAGMA busy_timeout = {BUSY_TIMEOUT_MS}"))
        .exec(&mut connection)
        .await?;

    let foreign_keys = toasty::sql::query("PRAGMA foreign_keys")
        .column_types([Type::I64])
        .exec(&mut connection)
        .await?;
    require_single_row("foreign_keys", &foreign_keys, &[Value::I64(1)])?;
    let busy_timeout = toasty::sql::query("PRAGMA busy_timeout")
        .column_types([Type::I64])
        .exec(&mut connection)
        .await?;
    require_single_row(
        "busy_timeout",
        &busy_timeout,
        &[Value::I64(BUSY_TIMEOUT_MS)],
    )?;
    Ok(connection)
}

fn require_single_row(context: &str, actual: &[Value], expected: &[Value]) -> toasty::Result<()> {
    if let [Value::Record(record)] = actual
        && record.as_slice() == expected
    {
        return Ok(());
    }
    Err(toasty::Error::from_args(format_args!(
        "unexpected SQLite row; context={context}, actual={actual:?}, expected={expected:?}"
    )))
}

async fn create_fts(executor: &mut dyn Executor) -> toasty::Result<()> {
    toasty::sql::statement(
        "CREATE VIRTUAL TABLE symbol_fts USING fts5(symbol_id UNINDEXED, name, content)",
    )
    .exec(executor)
    .await?;
    Ok(())
}

async fn insert_fts(
    executor: &mut dyn Executor,
    symbol_id: &str,
    name: &str,
    content: &str,
) -> toasty::Result<()> {
    toasty::sql::statement("INSERT INTO symbol_fts(symbol_id, name, content) VALUES (?1, ?2, ?3)")
        .bind(symbol_id)
        .bind(name)
        .bind(content)
        .exec(executor)
        .await?;
    Ok(())
}

async fn search_fts(executor: &mut dyn Executor, query: &str) -> toasty::Result<Vec<Value>> {
    toasty::sql::query(
        "SELECT symbol_id, name FROM symbol_fts WHERE symbol_fts MATCH ?1 ORDER BY rank LIMIT ?2",
    )
    .bind(query)
    .bind(FTS_QUERY_ROWS_MAX)
    .column_types([Type::String, Type::String])
    .exec(executor)
    .await
}

async fn migrate_and_reopen(path: &Path) -> toasty::Result<Db> {
    let (database, report) = open_database(path).await?;
    assert_eq!(report.applied(), 1, "fresh database must apply migration");
    assert_eq!(
        report.skipped(),
        0,
        "fresh database must not skip migration"
    );
    let mut connection = open_configured_connection(&database).await?;
    toasty::create!(SourceUnitRecord {
        id: "unit-restart",
        project_path: "src/restart.rs",
        revision: 1,
    })
    .exec(&mut connection)
    .await?;
    drop(connection);
    drop(database);

    let (database, report) = open_database(path).await?;
    assert_eq!(report.applied(), 0, "restart must not reapply migration");
    assert_eq!(report.skipped(), 1, "restart must retain migration history");
    let mut connection = open_configured_connection(&database).await?;
    let unit = SourceUnitRecord::get_by_id(&mut connection, "unit-restart").await?;
    assert_eq!(unit.revision, 1, "file restart must retain typed rows");
    Ok(database)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_toasty_sqlite_crud_batch_transaction_migration_and_restart() -> toasty::Result<()> {
    let directory = TempDir::new().map_err(toasty::Error::driver_operation_failed)?;
    let path = directory.path().join("index.db");
    let database = migrate_and_reopen(&path).await?;
    let mut connection = open_configured_connection(&database).await?;

    let mut unit = toasty::create!(SourceUnitRecord {
        id: "unit-main",
        project_path: "src/main.rs",
        revision: 1,
    })
    .exec(&mut connection)
    .await?;
    let found = SourceUnitRecord::get_by_id(&mut connection, "unit-main").await?;
    assert_eq!(
        found.project_path, "src/main.rs",
        "typed find must return created unit"
    );

    toasty::update!(unit { revision: 2 })
        .exec(&mut connection)
        .await?;
    let found = SourceUnitRecord::get_by_id(&mut connection, "unit-main").await?;
    assert_eq!(found.revision, 2, "typed update must persist revision");

    let symbols = SymbolRecord::create_many()
        .with_item(|create| {
            create
                .id("symbol-alpha")
                .source_unit_id("unit-main")
                .name("alpha")
                .content("fn alpha() {}")
        })
        .with_item(|create| {
            create
                .id("symbol-beta")
                .source_unit_id("unit-main")
                .name("beta")
                .content("fn beta() {}")
        })
        .exec(&mut connection)
        .await?;
    assert_eq!(symbols.len(), 2, "typed batch must create every symbol");

    symbols
        .into_iter()
        .next()
        .ok_or_else(|| toasty::Error::from_args(format_args!("batch returned no symbols")))?
        .delete()
        .exec(&mut connection)
        .await?;
    let remaining = SymbolRecord::filter_by_source_unit_id("unit-main")
        .exec(&mut connection)
        .await?;
    assert_eq!(remaining.len(), 1, "typed delete must remove one symbol");

    let mut transaction = connection.transaction().await?;
    toasty::create!(SymbolRecord {
        id: "symbol-rollback",
        source_unit_id: "unit-main",
        name: "rollback",
        content: "discarded",
    })
    .exec(&mut transaction)
    .await?;
    transaction.rollback().await?;
    let rolled_back = SymbolRecord::filter_by_id("symbol-rollback")
        .first()
        .exec(&mut connection)
        .await?;
    assert!(rolled_back.is_none(), "rollback must discard typed write");

    let orphan = toasty::create!(SymbolRecord {
        id: "symbol-orphan",
        source_unit_id: "unit-missing",
        name: "orphan",
        content: "invalid foreign key",
    })
    .exec(&mut connection)
    .await;
    assert!(
        orphan.is_err(),
        "foreign key must reject typed writes referencing missing source units"
    );
    SymbolRecord::filter_by_source_unit_id("unit-main")
        .delete()
        .exec(&mut connection)
        .await?;
    unit.delete().exec(&mut connection).await?;
    let deleted = SourceUnitRecord::filter_by_id("unit-main")
        .first()
        .exec(&mut connection)
        .await?;
    assert!(
        deleted.is_none(),
        "typed delete must remove unreferenced unit"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_toasty_sqlite_fts_is_atomic_and_rebuildable() -> toasty::Result<()> {
    let directory = TempDir::new().map_err(toasty::Error::driver_operation_failed)?;
    let path = directory.path().join("index.db");
    let (database, _) = open_database(&path).await?;
    let mut connection = open_configured_connection(&database).await?;
    create_fts(&mut connection).await?;
    toasty::create!(SourceUnitRecord {
        id: "unit-library",
        project_path: "src/lib.rs",
        revision: 1,
    })
    .exec(&mut connection)
    .await?;

    let mut transaction = connection.transaction().await?;
    toasty::create!(SymbolRecord {
        id: "symbol-discarded",
        source_unit_id: "unit-library",
        name: "discarded",
        content: "discarded transaction",
    })
    .exec(&mut transaction)
    .await?;
    insert_fts(
        &mut transaction,
        "symbol-discarded",
        "discarded",
        "discarded transaction",
    )
    .await?;
    transaction.rollback().await?;
    let discarded = SymbolRecord::filter_by_id("symbol-discarded")
        .first()
        .exec(&mut connection)
        .await?;
    assert!(discarded.is_none(), "typed row must roll back with FTS row");
    assert!(
        search_fts(&mut connection, "discarded").await?.is_empty(),
        "FTS row must roll back with typed row"
    );

    let mut transaction = connection.transaction().await?;
    toasty::create!(SymbolRecord {
        id: "symbol-parser",
        source_unit_id: "unit-library",
        name: "parse_document",
        content: "parse syntax tree",
    })
    .exec(&mut transaction)
    .await?;
    insert_fts(
        &mut transaction,
        "symbol-parser",
        "parse_document",
        "parse syntax tree",
    )
    .await?;
    transaction.commit().await?;
    require_single_row(
        "committed FTS match",
        &search_fts(&mut connection, "syntax").await?,
        &[
            Value::String("symbol-parser".to_owned()),
            Value::String("parse_document".to_owned()),
        ],
    )?;

    toasty::create!(SymbolRecord {
        id: "symbol-index",
        source_unit_id: "unit-library",
        name: "rebuild_index",
        content: "rebuild lexical index",
    })
    .exec(&mut connection)
    .await?;
    let mut transaction = connection.transaction().await?;
    toasty::sql::statement("DELETE FROM symbol_fts")
        .exec(&mut transaction)
        .await?;
    let symbols = SymbolRecord::all().exec(&mut transaction).await?;
    assert!(
        symbols.len() <= FTS_REBUILD_ROWS_MAX,
        "compatibility rebuild must stay bounded: rows={}, maximum={FTS_REBUILD_ROWS_MAX}",
        symbols.len()
    );
    for symbol in symbols {
        insert_fts(&mut transaction, &symbol.id, &symbol.name, &symbol.content).await?;
    }
    transaction.commit().await?;
    require_single_row(
        "rebuilt FTS match",
        &search_fts(&mut connection, "lexical").await?,
        &[
            Value::String("symbol-index".to_owned()),
            Value::String("rebuild_index".to_owned()),
        ],
    )?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_toasty_sqlite_wal_reads_committed_snapshot_during_write() -> toasty::Result<()> {
    let directory = TempDir::new().map_err(toasty::Error::driver_operation_failed)?;
    let path = directory.path().join("index.db");
    let (database, _) = open_database(&path).await?;
    let mut writer = open_configured_connection(&database).await?;
    let mut reader = open_configured_connection(&database).await?;
    toasty::create!(SourceUnitRecord {
        id: "unit-concurrent",
        project_path: "src/concurrent.rs",
        revision: 1,
    })
    .exec(&mut writer)
    .await?;

    let mut transaction = writer.transaction().await?;
    let mut unit = SourceUnitRecord::get_by_id(&mut transaction, "unit-concurrent").await?;
    toasty::update!(unit { revision: 2 })
        .exec(&mut transaction)
        .await?;
    let visible = SourceUnitRecord::get_by_id(&mut reader, "unit-concurrent").await?;
    assert_eq!(
        visible.revision, 1,
        "WAL reader must see committed snapshot while writer is active"
    );
    transaction.commit().await?;
    let visible = SourceUnitRecord::get_by_id(&mut reader, "unit-concurrent").await?;
    assert_eq!(visible.revision, 2, "reader must see committed revision");
    Ok(())
}
