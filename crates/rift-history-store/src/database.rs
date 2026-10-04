//! The store's `SQLite` tables: the filler's writes and the readers' queries.
//!
//! Every statement is prepared once per connection (`prepare_cached`), and
//! each write batch commits in one `BEGIN IMMEDIATE` transaction, so the
//! write lock is held for one batch at most and parsing never runs inside it.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rift_error::{RiftError, errors};
use rift_protocol::read::{CommitAuthor, SymbolVersionKind};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};

use crate::record::CommitRecord;

/// How long one connection waits for another's write lock before `SQLite`
/// reports the database busy.
const BUSY_TIMEOUT: Duration = Duration::from_millis(1_000);

/// The store's tables. A schema change ships in a new binary, whose
/// derivation revision names a new store file, so no migration ever runs.
fn schema() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS commits(
             row INTEGER PRIMARY KEY,
             id TEXT NOT NULL UNIQUE,
             base TEXT,
             boundary INTEGER NOT NULL,
             author_name TEXT NOT NULL,
             author_email TEXT NOT NULL,
             committed_at TEXT NOT NULL,
             time INTEGER NOT NULL,
             message TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS changed_paths(
             commit_row INTEGER NOT NULL,
             path TEXT NOT NULL,
             old_blob TEXT,
             new_blob TEXT
         );
         CREATE INDEX IF NOT EXISTS changed_paths_commit ON changed_paths(commit_row);
         CREATE INDEX IF NOT EXISTS changed_paths_path ON changed_paths(path);
         CREATE TABLE IF NOT EXISTS renamed_paths(
             commit_row INTEGER NOT NULL,
             old_path TEXT NOT NULL,
             new_path TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS renamed_paths_commit ON renamed_paths(commit_row, new_path);
         CREATE TABLE IF NOT EXISTS moved_declarations(
             commit_row INTEGER NOT NULL,
             new_path TEXT NOT NULL,
             qualified_name TEXT NOT NULL,
             old_path TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS moved_declarations_commit
             ON moved_declarations(commit_row, new_path, qualified_name);
         CREATE TABLE IF NOT EXISTS changed_declarations(
             commit_row INTEGER NOT NULL,
             path TEXT NOT NULL,
             qualified_name TEXT NOT NULL,
             change TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS changed_declarations_commit
             ON changed_declarations(commit_row, path, qualified_name);
         CREATE INDEX IF NOT EXISTS changed_declarations_name
             ON changed_declarations(qualified_name);
         CREATE VIRTUAL TABLE IF NOT EXISTS commit_text USING fts5(
             message, content='commits', content_rowid='row', tokenize='{tokenizer}'
         );",
        tokenizer = rift_ranking::CORPUS_TOKENIZER,
    )
}

/// The tables whose rows belong to one commit row.
const COMMIT_CHILD_TABLES: [&str; 4] = [
    "changed_paths",
    "renamed_paths",
    "moved_declarations",
    "changed_declarations",
];

/// Opens one connection to `database` in WAL mode, where a writer never
/// blocks a reader.
fn connect(database: &Path) -> Result<Connection, RiftError> {
    let connection = Connection::open(database).map_err(|source| {
        errors::history_store::database()
            .operation("open store")
            .detail(source)
            .error()
    })?;
    connection.busy_timeout(BUSY_TIMEOUT).map_err(|source| {
        errors::history_store::database()
            .operation("set busy timeout")
            .detail(source)
            .error()
    })?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(|source| {
            errors::history_store::database()
                .operation("enter WAL mode")
                .detail(source)
                .error()
        })?;
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(|source| {
            errors::history_store::database()
                .operation("set synchronous mode")
                .detail(source)
                .error()
        })?;
    Ok(connection)
}

/// Creates the store's tables when the file lacks them.
pub(crate) fn create_schema(database: &Path) -> Result<(), RiftError> {
    connect(database)?
        .execute_batch(&schema())
        .map_err(|source| {
            errors::history_store::database()
                .operation("create store tables")
                .detail(source)
                .error()
        })
}

/// What the store holds for one commit: what it was compared with, and
/// whether history past it is out of reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldCommit {
    /// The commit it was compared with.
    pub base: Option<String>,
    /// Whether history past it is out of the store's reach.
    pub boundary: bool,
}

/// The one writer of a store: the fill lock and a write connection.
#[derive(Debug)]
pub struct StoreFiller {
    connection: Connection,
    _fill: File,
}

impl StoreFiller {
    /// Opens the write connection while `fill` holds the fill lock.
    pub(crate) fn open(database: &Path, fill: File) -> Result<Self, RiftError> {
        Ok(Self {
            connection: connect(database)?,
            _fill: fill,
        })
    }

    /// The write connection, so a test reaches the tables directly.
    #[cfg(test)]
    pub(crate) const fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Every commit the store holds, keyed by commit id.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn held(&self) -> Result<HashMap<String, HeldCommit>, RiftError> {
        held_commits(&self.connection)
    }

    /// Writes one batch in one `BEGIN IMMEDIATE` transaction. A commit the
    /// store already holds under another base is replaced whole.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses a statement; the batch then
    /// writes nothing.
    pub fn write_batch(&mut self, batch: &[CommitRecord]) -> Result<(), RiftError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| {
                errors::history_store::database()
                    .operation("begin batch")
                    .detail(source)
                    .error()
            })?;
        for commit in batch {
            let held: Option<i64> = transaction
                .prepare_cached("SELECT row FROM commits WHERE id = ?1")
                .and_then(|mut statement| {
                    statement
                        .query_row([&commit.id], |row| row.get(0))
                        .optional()
                })
                .map_err(|source| {
                    errors::history_store::database()
                        .operation("read held commit")
                        .detail(source)
                        .error()
                })?;
            if let Some(row) = held {
                delete_commit(&transaction, row)?;
            }
            write_commit(&transaction, commit)?;
        }
        transaction.commit().map_err(|source| {
            errors::history_store::database()
                .operation("commit batch")
                .detail(source)
                .error()
        })
    }

    /// Deletes every commit outside `keep`, index entries first. Returns how
    /// many commits went.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses a statement; the trim then
    /// deletes nothing.
    pub fn trim(&mut self, keep: &BTreeSet<String>) -> Result<usize, RiftError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| {
                errors::history_store::database()
                    .operation("begin trim")
                    .detail(source)
                    .error()
            })?;
        let held: Vec<(i64, String)> = transaction
            .prepare_cached("SELECT row, id FROM commits")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect()
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("read held commits")
                    .detail(source)
                    .error()
            })?;
        let mut deleted = 0_usize;
        for (row, _) in held.iter().filter(|(_, id)| !keep.contains(id)) {
            delete_commit(&transaction, *row)?;
            deleted += 1;
        }
        transaction.commit().map_err(|source| {
            errors::history_store::database()
                .operation("commit trim")
                .detail(source)
                .error()
        })?;
        Ok(deleted)
    }

    /// Checks the message index against the commit rows it names: `SQLite`'s
    /// full-text `integrity-check` with `rank` 1 compares the two.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the index and the commit rows disagree.
    pub fn check_message_index(&self) -> Result<(), RiftError> {
        self.connection
            .execute(
                "INSERT INTO commit_text(commit_text, rank) VALUES('integrity-check', 1)",
                [],
            )
            .map(|_| ())
            .map_err(|source| {
                errors::history_store::database()
                    .operation("check message index")
                    .detail(source)
                    .error()
            })
    }
}

/// Every commit the store `connection` reads holds, keyed by commit id.
fn held_commits(connection: &Connection) -> Result<HashMap<String, HeldCommit>, RiftError> {
    let mut statement = connection
        .prepare_cached("SELECT id, base, boundary FROM commits")
        .map_err(|source| {
            errors::history_store::database()
                .operation("read held commits")
                .detail(source)
                .error()
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                HeldCommit {
                    base: row.get(1)?,
                    boundary: row.get(2)?,
                },
            ))
        })
        .map_err(|source| {
            errors::history_store::database()
                .operation("read held commits")
                .detail(source)
                .error()
        })?;
    rows.collect::<Result<_, _>>().map_err(|source| {
        errors::history_store::database()
            .operation("read held commits")
            .detail(source)
            .error()
    })
}

/// Writes one commit's rows, the message index entry after the commit row.
fn write_commit(
    transaction: &rusqlite::Transaction<'_>,
    commit: &CommitRecord,
) -> Result<(), RiftError> {
    transaction
        .prepare_cached(
            "INSERT INTO commits(id, base, boundary, author_name, author_email, committed_at, \
             time, message) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )
        .and_then(|mut statement| {
            statement.execute(params![
                commit.id,
                commit.base,
                commit.boundary,
                commit.author.name,
                commit.author.email,
                commit.committed_at,
                commit.time,
                commit.message
            ])
        })
        .map_err(|source| {
            errors::history_store::database()
                .operation("write commit")
                .detail(source)
                .error()
        })?;
    let row = transaction.last_insert_rowid();
    transaction
        .prepare_cached("INSERT INTO commit_text(rowid, message) VALUES (?1, ?2)")
        .and_then(|mut statement| statement.execute(params![row, commit.message]))
        .map_err(|source| {
            errors::history_store::database()
                .operation("index commit message")
                .detail(source)
                .error()
        })?;
    write_commit_paths_and_declarations(transaction, row, commit)?;
    Ok(())
}

/// Writes path and declaration rows that belong to one commit row.
fn write_commit_paths_and_declarations(
    transaction: &rusqlite::Transaction<'_>,
    row: i64,
    commit: &CommitRecord,
) -> Result<(), RiftError> {
    for changed in &commit.paths {
        transaction
            .prepare_cached("INSERT INTO changed_paths VALUES (?1, ?2, ?3, ?4)")
            .and_then(|mut statement| {
                statement.execute(params![
                    row,
                    changed.path,
                    changed.old_blob,
                    changed.new_blob
                ])
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("write changed path")
                    .detail(source)
                    .error()
            })?;
    }
    for renamed in &commit.renames {
        transaction
            .prepare_cached("INSERT INTO renamed_paths VALUES (?1, ?2, ?3)")
            .and_then(|mut statement| {
                statement.execute(params![row, renamed.old_path, renamed.new_path])
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("write renamed path")
                    .detail(source)
                    .error()
            })?;
    }
    for moved in &commit.moves {
        transaction
            .prepare_cached("INSERT INTO moved_declarations VALUES (?1, ?2, ?3, ?4)")
            .and_then(|mut statement| {
                statement.execute(params![
                    row,
                    moved.new_path,
                    moved.qualified_name,
                    moved.old_path
                ])
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("write moved declaration")
                    .detail(source)
                    .error()
            })?;
    }
    for declaration in &commit.declarations {
        let change = change_code(declaration.change);
        transaction
            .prepare_cached("INSERT INTO changed_declarations VALUES (?1, ?2, ?3, ?4)")
            .and_then(|mut statement| {
                statement.execute(params![
                    row,
                    declaration.path,
                    declaration.qualified_name,
                    change
                ])
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("write changed declaration")
                    .detail(source)
                    .error()
            })?;
    }
    Ok(())
}

/// Deletes one commit row and every row that belongs to it. The message
/// index entry goes first, while the commit row still holds the message it
/// was indexed from.
fn delete_commit(transaction: &rusqlite::Transaction<'_>, row: i64) -> Result<(), RiftError> {
    transaction
        .prepare_cached(
            "INSERT INTO commit_text(commit_text, rowid, message) \
             SELECT 'delete', row, message FROM commits WHERE row = ?1",
        )
        .and_then(|mut statement| statement.execute([row]))
        .map_err(|source| {
            errors::history_store::database()
                .operation("delete message index entry")
                .detail(source)
                .error()
        })?;
    for table in COMMIT_CHILD_TABLES {
        transaction
            .prepare_cached(&format!("DELETE FROM {table} WHERE commit_row = ?1"))
            .and_then(|mut statement| statement.execute([row]))
            .map_err(|source| {
                errors::history_store::database()
                    .operation("delete commit rows")
                    .detail(source)
                    .error()
            })?;
    }
    transaction
        .prepare_cached("DELETE FROM commits WHERE row = ?1")
        .and_then(|mut statement| statement.execute([row]))
        .map_err(|source| {
            errors::history_store::database()
                .operation("delete commit")
                .detail(source)
                .error()
        })?;
    Ok(())
}

/// The stored spelling of a change: its wire spelling, which serde owns.
fn change_code(change: SymbolVersionKind) -> String {
    match serde_json::to_value(change) {
        Ok(serde_json::Value::String(code)) => code,
        other => unreachable!("a version kind serializes as a string: {other:?}"),
    }
}

/// The change a stored spelling names; `None` for a spelling no variant
/// carries.
fn stored_change(code: String) -> Option<SymbolVersionKind> {
    serde_json::from_value(serde_json::Value::String(code)).ok()
}

/// Opens read connections to one store file.
#[derive(Clone, Debug)]
pub struct StoreReader {
    database: Arc<Path>,
}

impl StoreReader {
    pub(crate) fn new(database: PathBuf) -> Self {
        Self {
            database: Arc::from(database),
        }
    }

    /// Opens one read connection. A connection answers one request's reads;
    /// WAL mode keeps it off the filler's write lock.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the file.
    pub fn connect(&self) -> Result<StoreReads, RiftError> {
        let connection = connect(&self.database)?;
        connection
            .pragma_update(None, "query_only", true)
            .map_err(|source| {
                errors::history_store::database()
                    .operation("enter read-only mode")
                    .detail(source)
                    .error()
            })?;
        Ok(StoreReads { connection })
    }
}

/// One commit as the store holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredCommit {
    row: i64,
    /// The commit id, full lowercase hex.
    pub id: String,
    /// The commit it was compared with.
    pub base: Option<String>,
    /// Whether history past it is out of the store's reach.
    pub boundary: bool,
    /// The author the commit records.
    pub author: CommitAuthor,
    /// The committer time, an RFC 3339 date-time carrying the recorded
    /// offset.
    pub committed_at: String,
    /// The full commit message.
    pub message: String,
}

impl StoredCommit {
    /// The message's first line; `None` for an empty one.
    #[must_use]
    pub fn summary(&self) -> Option<&str> {
        self.message
            .lines()
            .next()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
    }
}

/// One read connection's queries.
#[derive(Debug)]
pub struct StoreReads {
    connection: Connection,
}

impl StoreReads {
    /// The read connection, so a test reaches the tables directly.
    #[cfg(test)]
    pub(crate) const fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Every commit the store holds, keyed by commit id, as the filler reads
    /// them: a server whose task does not hold the fill lock plans against
    /// this to know how far another server's fill has got.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn held(&self) -> Result<HashMap<String, HeldCommit>, RiftError> {
        held_commits(&self.connection)
    }

    /// The commit `id` names, when the store holds it.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn commit(&self, id: &str) -> Result<Option<StoredCommit>, RiftError> {
        self.connection
            .prepare_cached(
                "SELECT row, id, base, boundary, author_name, author_email, committed_at, message \
                 FROM commits WHERE id = ?1",
            )
            .and_then(|mut statement| {
                statement
                    .query_row([id], |row| {
                        Ok(StoredCommit {
                            row: row.get(0)?,
                            id: row.get(1)?,
                            base: row.get(2)?,
                            boundary: row.get(3)?,
                            author: CommitAuthor {
                                name: row.get(4)?,
                                email: row.get(5)?,
                            },
                            committed_at: row.get(6)?,
                            message: row.get(7)?,
                        })
                    })
                    .optional()
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("read commit")
                    .detail(source)
                    .error()
            })
    }

    /// How `commit` changed the declaration `qualified_name` at `path`, when
    /// it changed it.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn declaration_change(
        &self,
        commit: &StoredCommit,
        path: &str,
        qualified_name: &str,
    ) -> Result<Option<SymbolVersionKind>, RiftError> {
        let code: Option<String> = self
            .connection
            .prepare_cached(
                "SELECT change FROM changed_declarations \
                 WHERE commit_row = ?1 AND path = ?2 AND qualified_name = ?3",
            )
            .and_then(|mut statement| {
                statement
                    .query_row(params![commit.row, path, qualified_name], |row| row.get(0))
                    .optional()
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("read declaration change")
                    .detail(source)
                    .error()
            })?;
        Ok(code.and_then(stored_change))
    }

    /// The path `commit` moved the file now at `new_path` from, when it moved
    /// it without changing its bytes.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn renamed_from(
        &self,
        commit: &StoredCommit,
        new_path: &str,
    ) -> Result<Option<String>, RiftError> {
        self.connection
            .prepare_cached(
                "SELECT old_path FROM renamed_paths WHERE commit_row = ?1 AND new_path = ?2",
            )
            .and_then(|mut statement| {
                statement
                    .query_row(params![commit.row, new_path], |row| row.get(0))
                    .optional()
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("read renamed path")
                    .detail(source)
                    .error()
            })
    }

    /// The path `commit` moved the declaration `qualified_name` now at `new_path`
    /// from, when it moved it between files that were no pure rename.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn moved_from(
        &self,
        commit: &StoredCommit,
        new_path: &str,
        qualified_name: &str,
    ) -> Result<Option<String>, RiftError> {
        self.connection
            .prepare_cached(
                "SELECT old_path FROM moved_declarations \
                 WHERE commit_row = ?1 AND new_path = ?2 AND qualified_name = ?3",
            )
            .and_then(|mut statement| {
                statement
                    .query_row(params![commit.row, new_path, qualified_name], |row| {
                        row.get(0)
                    })
                    .optional()
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("read moved declaration")
                    .detail(source)
                    .error()
            })
    }

    /// The paths `commit` changed against the commit it was compared with, in path
    /// order, at most `limit` of them.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn changed_paths(
        &self,
        commit: &StoredCommit,
        limit: usize,
    ) -> Result<Vec<String>, RiftError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.connection
            .prepare_cached(
                "SELECT path FROM changed_paths WHERE commit_row = ?1 ORDER BY path LIMIT ?2",
            )
            .and_then(|mut statement| {
                statement
                    .query_map(params![commit.row, limit], |row| row.get(0))?
                    .collect()
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("read changed paths")
                    .detail(source)
                    .error()
            })
    }

    /// The held commit no other held commit was compared with, newest first:
    /// the newest release a release chain starts at.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the read.
    pub fn chain_head(&self) -> Result<Option<String>, RiftError> {
        self.connection
            .prepare_cached(
                "SELECT id FROM commits WHERE id NOT IN \
                 (SELECT base FROM commits WHERE base IS NOT NULL) \
                 ORDER BY time DESC, id LIMIT 1",
            )
            .and_then(|mut statement| statement.query_row([], |row| row.get(0)).optional())
            .map_err(|source| {
                errors::history_store::database()
                    .operation("read chain head")
                    .detail(source)
                    .error()
            })
    }

    /// The ids of the commits whose message matches the full-text `query`,
    /// newest first, at most `limit` of them.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when `SQLite` refuses the query, a malformed
    /// full-text query included.
    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<String>, RiftError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.connection
            .prepare_cached(
                "SELECT commits.id FROM commit_text \
                 JOIN commits ON commits.row = commit_text.rowid \
                 WHERE commit_text MATCH ?1 ORDER BY commits.time DESC, commits.id LIMIT ?2",
            )
            .and_then(|mut statement| {
                statement
                    .query_map(params![query, limit], |row| row.get(0))?
                    .collect()
            })
            .map_err(|source| {
                errors::history_store::database()
                    .operation("search commit messages")
                    .detail(source)
                    .error()
            })
    }
}
