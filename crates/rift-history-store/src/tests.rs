use std::collections::BTreeSet;
use std::error::Error;
use std::path::Path;

use rift_error::errors;
use rift_protocol::read::{CommitAuthor, SymbolVersionKind};

use crate::lock::{LIVE_LOCK_ATTEMPTS_MAX, lock_live_checked};
use crate::{
    ChangedPath, CommitRecord, DeclarationChange, HistoryStore, MovedDeclaration, RenamedPath,
    STORE_FOLDER_NAME, StoreLocation,
};

type TestResult = Result<(), Box<dyn Error>>;

fn changed(path: &str, old: Option<&str>, new: Option<&str>) -> ChangedPath {
    ChangedPath {
        path: path.to_owned(),
        old_blob: old.map(str::to_owned),
        new_blob: new.map(str::to_owned),
    }
}

/// One commit changing `src/lib.rs`'s `parse` and moving `old.rs` to `new.rs`.
fn commit(id: &str, base: Option<&str>, time: i64, message: &str) -> CommitRecord {
    let paths = vec![
        changed("src/lib.rs", Some("aa"), Some(id)),
        changed("old.rs", Some("bb"), None),
        changed("new.rs", None, Some("bb")),
    ];
    let renames = ChangedPath::pure_renames(&paths);
    CommitRecord {
        id: id.to_owned(),
        base: base.map(str::to_owned),
        boundary: false,
        author: CommitAuthor {
            name: "Rift Fixture".to_owned(),
            email: "fixture@rift.invalid".to_owned(),
        },
        committed_at: format!("2026-01-01T00:00:{time:02}+00:00"),
        time,
        message: message.to_owned(),
        paths,
        renames,
        moves: vec![MovedDeclaration {
            old_path: "src/old_home.rs".to_owned(),
            new_path: "src/lib.rs".to_owned(),
            qualified_name: "helper".to_owned(),
        }],
        declarations: vec![DeclarationChange {
            path: "src/lib.rs".to_owned(),
            qualified_name: "parse".to_owned(),
            change: SymbolVersionKind::BodyChanged,
        }],
    }
}

/// A store at `revision` in `folder`, filled with three commits on one chain.
fn filled(folder: &Path, revision: &str) -> Result<HistoryStore, Box<dyn Error>> {
    let store = HistoryStore::open(&StoreLocation::new(folder, revision))?;
    let mut filler = store.filler()?.ok_or("the first filler takes the lock")?;
    filler.write_batch(&[
        commit("c1", None, 10, "Add lexical search\n\nThe body."),
        commit("c2", Some("c1"), 20, "Fix release notes"),
        commit("c3", Some("c2"), 30, "Fix lexical ranking"),
    ])?;
    Ok(store)
}

fn sqlite_refusal<'error>(
    error: &'error rift_error::RiftError,
    operation: &str,
) -> &'error rusqlite::Error {
    assert_eq!(error.slug(), errors::history_store::database::SLUG);
    assert_eq!(error.action(), "check the history store database and retry");
    assert_eq!(
        error
            .context()
            .find(|(key, _)| *key == "operation")
            .map(|(_, value)| value)
            .as_deref(),
        Some(operation)
    );
    let source = error
        .source()
        .expect("SQLite failure is retained")
        .downcast_ref::<rusqlite::Error>()
        .expect("original SQLite error type");
    assert!(error.message().contains(&source.to_string()));
    source
}

fn stored_row_counts(connection: &rusqlite::Connection) -> rusqlite::Result<Vec<i64>> {
    [
        "commits",
        "changed_paths",
        "renamed_paths",
        "moved_declarations",
        "changed_declarations",
    ]
    .into_iter()
    .map(|table| {
        connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
    })
    .collect()
}

#[test]
fn missing_store_tables_retain_each_read_operation_and_sqlite_source() -> TestResult {
    let folder = tempfile::tempdir()?;
    let store = filled(folder.path(), "aa")?;
    let reads = store.reader().connect()?;
    let head = reads.commit("c3")?.ok_or("c3 is held")?;
    let mut filler = store.filler()?.ok_or("no other filler runs")?;
    filler.connection().execute_batch(
        "DROP TABLE changed_declarations; DROP TABLE renamed_paths;
         DROP TABLE moved_declarations; DROP TABLE changed_paths;",
    )?;
    for (error, operation, table) in [
        (
            reads
                .declaration_change(&head, "src/lib.rs", "parse")
                .expect_err("missing table"),
            "read declaration change",
            "changed_declarations",
        ),
        (
            reads
                .renamed_from(&head, "new.rs")
                .expect_err("missing table"),
            "read renamed path",
            "renamed_paths",
        ),
        (
            reads
                .moved_from(&head, "src/lib.rs", "helper")
                .expect_err("missing table"),
            "read moved declaration",
            "moved_declarations",
        ),
        (
            reads.changed_paths(&head, 10).expect_err("missing table"),
            "read changed paths",
            "changed_paths",
        ),
    ] {
        assert_eq!(
            sqlite_refusal(&error, operation).to_string(),
            format!("no such table: {table}")
        );
    }
    filler.connection().execute_batch("DROP TABLE commits")?;
    for (error, operation) in [
        (
            reads.commit("c3").expect_err("missing table"),
            "read commit",
        ),
        (
            reads.chain_head().expect_err("missing table"),
            "read chain head",
        ),
        (
            reads.held().expect_err("missing table"),
            "read held commits",
        ),
        (
            filler
                .write_batch(&[commit("c4", Some("c3"), 40, "New commit")])
                .expect_err("missing table"),
            "read held commit",
        ),
        (
            filler.trim(&BTreeSet::new()).expect_err("missing table"),
            "read held commits",
        ),
    ] {
        assert_eq!(
            sqlite_refusal(&error, operation).to_string(),
            "no such table: commits"
        );
    }
    Ok(())
}

#[test]
fn invalid_stored_column_retains_conversion_error_for_commit_and_held_reads() -> TestResult {
    let folder = tempfile::tempdir()?;
    let store = filled(folder.path(), "aa")?;
    let filler = store.filler()?.ok_or("no other filler runs")?;
    filler
        .connection()
        .execute("UPDATE commits SET boundary = x'00' WHERE id = 'c2'", [])?;
    let reads = store.reader().connect()?;
    for (error, operation) in [
        (
            reads.commit("c2").expect_err("blob is not a boolean"),
            "read commit",
        ),
        (
            reads.held().expect_err("blob is not a boolean"),
            "read held commits",
        ),
        (
            filler.held().expect_err("blob is not a boolean"),
            "read held commits",
        ),
    ] {
        assert!(
            matches!(sqlite_refusal(&error, operation), rusqlite::Error::InvalidColumnType(_, name, rusqlite::types::Type::Blob) if name == "boundary")
        );
    }
    assert!(
        reads.commit("c1")?.is_some(),
        "other stored rows remain readable"
    );
    Ok(())
}

#[test]
fn refused_child_writes_roll_back_prior_commits_and_message_index_entries() -> TestResult {
    for (table, operation) in [
        ("changed_paths", "write changed path"),
        ("renamed_paths", "write renamed path"),
        ("moved_declarations", "write moved declaration"),
        ("changed_declarations", "write changed declaration"),
    ] {
        let folder = tempfile::tempdir()?;
        let store = filled(folder.path(), "aa")?;
        let mut filler = store.filler()?.ok_or("no other filler runs")?;
        let before = stored_row_counts(filler.connection())?;
        filler.connection().execute_batch(&format!(
            "CREATE TRIGGER refuse_child BEFORE INSERT ON {table}
             WHEN NEW.commit_row = (SELECT row FROM commits WHERE id = 'c5')
             BEGIN SELECT RAISE(ABORT, 'refused child write'); END;"
        ))?;
        let error = filler
            .write_batch(&[
                commit("c4", Some("c3"), 40, "Unpublished fourth"),
                commit("c5", Some("c4"), 50, "Unpublished fifth"),
            ])
            .expect_err("second commit child write is refused");
        let source = sqlite_refusal(&error, operation);
        assert!(
            matches!(source, rusqlite::Error::SqliteFailure(_, Some(message)) if message == "refused child write")
        );
        assert_eq!(stored_row_counts(filler.connection())?, before);
        filler.check_message_index()?;
        let reads = store.reader().connect()?;
        assert!(reads.commit("c4")?.is_none());
        assert!(reads.commit("c5")?.is_none());
        assert!(reads.search_messages("Unpublished", 10)?.is_empty());
        assert_eq!(reads.search_messages("lexical", 10)?, ["c3", "c1"]);
    }
    Ok(())
}

#[test]
fn refused_child_deletes_roll_back_trim_and_commit_replacement() -> TestResult {
    for table in [
        "changed_paths",
        "renamed_paths",
        "moved_declarations",
        "changed_declarations",
    ] {
        let folder = tempfile::tempdir()?;
        let store = filled(folder.path(), "aa")?;
        let mut filler = store.filler()?.ok_or("no other filler runs")?;
        let before = stored_row_counts(filler.connection())?;
        filler.connection().execute_batch(&format!(
            "CREATE TRIGGER refuse_child BEFORE DELETE ON {table}
             WHEN OLD.commit_row = (SELECT row FROM commits WHERE id = 'c2')
             BEGIN SELECT RAISE(ABORT, 'refused child delete'); END;"
        ))?;
        let trim = filler
            .trim(&BTreeSet::new())
            .expect_err("child delete is refused");
        assert_eq!(
            sqlite_refusal(&trim, "delete commit rows").to_string(),
            "refused child delete"
        );
        assert_eq!(stored_row_counts(filler.connection())?, before);
        filler.check_message_index()?;

        let replace = filler
            .write_batch(&[commit("c2", Some("c1"), 20, "Replacement")])
            .expect_err("replacement child delete is refused");
        assert_eq!(
            sqlite_refusal(&replace, "delete commit rows").to_string(),
            "refused child delete"
        );
        assert_eq!(stored_row_counts(filler.connection())?, before);
        filler.check_message_index()?;
        let reads = store.reader().connect()?;
        assert_eq!(reads.search_messages("release", 10)?, ["c2"]);
        assert!(reads.search_messages("Replacement", 10)?.is_empty());
    }
    Ok(())
}

#[test]
fn missing_message_index_refuses_batch_and_trim_without_changing_rows() -> TestResult {
    let folder = tempfile::tempdir()?;
    let store = filled(folder.path(), "aa")?;
    let mut filler = store.filler()?.ok_or("no other filler runs")?;
    let before = stored_row_counts(filler.connection())?;
    filler
        .connection()
        .execute_batch("DROP TABLE commit_text")?;
    let write = filler
        .write_batch(&[commit("c4", Some("c3"), 40, "New commit")])
        .expect_err("message index is absent");
    assert_eq!(
        sqlite_refusal(&write, "index commit message").to_string(),
        "no such table: commit_text"
    );
    assert_eq!(stored_row_counts(filler.connection())?, before);
    let trim = filler
        .trim(&BTreeSet::new())
        .expect_err("message index is absent");
    assert_eq!(
        sqlite_refusal(&trim, "delete message index entry").to_string(),
        "no such table: commit_text"
    );
    assert_eq!(stored_row_counts(filler.connection())?, before);
    Ok(())
}

#[test]
fn incompatible_store_table_refuses_schema_creation_with_sqlite_source() -> TestResult {
    let folder = tempfile::tempdir()?;
    let location = StoreLocation::new(folder.path(), "aa");
    std::fs::create_dir_all(location.folder())?;
    let connection = rusqlite::Connection::open(location.database())?;
    connection.execute_batch("CREATE TABLE changed_paths(unrelated TEXT)")?;
    drop(connection);
    let error = HistoryStore::open(&location).expect_err("required column is missing");
    assert!(
        sqlite_refusal(&error, "create store tables")
            .to_string()
            .contains("no such column: commit_row")
    );
    Ok(())
}

#[test]
fn pure_renames_pair_one_to_one_blob_ids_only() {
    let paths = [
        changed("a.rs", Some("1"), None),
        changed("b.rs", None, Some("1")),
        changed("c.rs", Some("2"), Some("3")),
    ];
    assert_eq!(
        ChangedPath::pure_renames(&paths),
        vec![RenamedPath {
            old_path: "a.rs".to_owned(),
            new_path: "b.rs".to_owned()
        }]
    );
    // Two additions carry the deleted blob id, so the id pairs no rename.
    let ambiguous = [
        changed("x/__init__.py", Some("0"), None),
        changed("y/__init__.py", None, Some("0")),
        changed("z/__init__.py", None, Some("0")),
    ];
    assert!(ChangedPath::pure_renames(&ambiguous).is_empty());
}

#[test]
fn a_reader_answers_commits_declarations_renames_and_message_terms() -> TestResult {
    let folder = tempfile::tempdir()?;
    let store = filled(folder.path(), "aa")?;
    let reads = store.reader().connect()?;

    let head = reads.commit("c3")?.ok_or("c3 is held")?;
    assert_eq!(head.base.as_deref(), Some("c2"));
    assert!(!head.boundary);
    assert_eq!(head.author.name, "Rift Fixture");
    assert_eq!(head.author.email, "fixture@rift.invalid");
    assert_eq!(head.summary(), Some("Fix lexical ranking"));
    assert_eq!(
        reads.declaration_change(&head, "src/lib.rs", "parse")?,
        Some(SymbolVersionKind::BodyChanged)
    );
    assert_eq!(
        reads.declaration_change(&head, "src/lib.rs", "other")?,
        None
    );
    assert_eq!(
        reads.renamed_from(&head, "new.rs")?.as_deref(),
        Some("old.rs")
    );
    assert_eq!(reads.renamed_from(&head, "src/lib.rs")?, None);
    assert_eq!(
        reads.moved_from(&head, "src/lib.rs", "helper")?.as_deref(),
        Some("src/old_home.rs")
    );
    assert_eq!(reads.moved_from(&head, "src/lib.rs", "parse")?, None);
    assert!(reads.commit("absent")?.is_none());

    let root = reads.commit("c1")?.ok_or("c1 is held")?;
    assert_eq!(root.message, "Add lexical search\n\nThe body.");
    assert_eq!(root.summary(), Some("Add lexical search"));

    assert_eq!(reads.search_messages("lexical", 10)?, ["c3", "c1"]);
    assert_eq!(reads.search_messages("release", 10)?, ["c2"]);
    assert_eq!(reads.search_messages("lexical", 1)?, ["c3"]);
    assert_eq!(reads.chain_head()?.as_deref(), Some("c3"));
    Ok(())
}

#[test]
fn changed_paths_answer_in_path_order_up_to_the_limit() -> TestResult {
    let folder = tempfile::tempdir()?;
    let store = filled(folder.path(), "aa")?;
    let reads = store.reader().connect()?;
    let commit = reads.commit("c2")?.ok_or("c2 is held")?;

    assert_eq!(
        reads.changed_paths(&commit, 10)?,
        ["new.rs", "old.rs", "src/lib.rs"]
    );
    assert_eq!(reads.changed_paths(&commit, 2)?, ["new.rs", "old.rs"]);
    assert!(reads.changed_paths(&commit, 0)?.is_empty());
    Ok(())
}

#[test]
fn a_filler_reports_what_it_holds_and_replaces_a_commit_held_under_another_base() -> TestResult {
    let folder = tempfile::tempdir()?;
    let store = filled(folder.path(), "aa")?;
    let mut filler = store.filler()?.ok_or("no other filler runs")?;
    let held = filler.held()?;
    assert_eq!(held.len(), 3);
    assert_eq!(held["c2"].base.as_deref(), Some("c1"));

    let mut replaced = commit("c2", None, 20, "Fix release notes again");
    replaced.boundary = true;
    filler.write_batch(&[replaced])?;

    let held = filler.held()?;
    assert_eq!(held.len(), 3);
    assert_eq!(held["c2"].base, None);
    assert!(held["c2"].boundary);
    let reads = store.reader().connect()?;
    assert_eq!(reads.search_messages("again", 10)?, ["c2"]);
    filler.check_message_index()?;
    Ok(())
}

#[test]
fn trim_keeps_the_window_union_and_the_message_index_stays_valid() -> TestResult {
    let folder = tempfile::tempdir()?;
    let store = filled(folder.path(), "aa")?;
    let mut filler = store.filler()?.ok_or("no other filler runs")?;
    let keep: BTreeSet<String> = ["c2".to_owned(), "c3".to_owned()].into_iter().collect();

    assert_eq!(filler.trim(&keep)?, 1);
    assert_eq!(filler.trim(&keep)?, 0, "a second trim deletes nothing");

    let reads = store.reader().connect()?;
    assert_eq!(reads.search_messages("lexical", 10)?, ["c3"]);
    assert!(reads.commit("c1")?.is_none());
    filler.check_message_index()?;
    let rows: i64 =
        filler
            .connection()
            .query_row("SELECT COUNT(*) FROM changed_declarations", [], |row| {
                row.get(0)
            })?;
    assert_eq!(rows, 2);
    Ok(())
}

#[test]
fn a_delete_without_the_index_command_fails_the_integrity_check() -> TestResult {
    let folder = tempfile::tempdir()?;
    let store = filled(folder.path(), "aa")?;
    let filler = store.filler()?.ok_or("no other filler runs")?;

    filler
        .connection()
        .execute("DELETE FROM commits WHERE id = 'c1'", [])?;

    let refused = filler
        .check_message_index()
        .expect_err("the index names a row the table no longer holds");
    assert_eq!(refused.slug(), errors::history_store::database::SLUG);
    Ok(())
}

#[test]
fn a_second_filler_is_refused_while_readers_read() -> TestResult {
    let folder = tempfile::tempdir()?;
    let location = StoreLocation::new(folder.path(), "aa");
    let first = filled(folder.path(), "aa")?;
    let _filler = first.filler()?.ok_or("the first filler takes the lock")?;

    let second = HistoryStore::open(&location)?;
    assert!(second.filler()?.is_none(), "one filler at a time");
    let reads = second.reader().connect()?;
    assert_eq!(reads.search_messages("fix", 10)?.len(), 2);
    let refused = reads.connection().execute("DELETE FROM commits", []);
    assert!(refused.is_err(), "a read connection writes nothing");
    Ok(())
}

#[test]
fn sweep_deletes_only_revisions_no_live_server_holds() -> TestResult {
    let folder = tempfile::tempdir()?;
    let rift = folder.path().join(STORE_FOLDER_NAME);
    let held = HistoryStore::open(&StoreLocation::new(folder.path(), "bb"))?;
    drop(filled(folder.path(), "aa")?);
    let current = HistoryStore::open(&StoreLocation::new(folder.path(), "cc"))?;

    let swept = current.sweep()?;
    assert_eq!(swept.deleted(), ["aa"]);
    assert!(swept.failures().is_empty());
    assert!(!rift.join("store-aa.db").exists());
    assert!(!rift.join("store-aa.live.lock").exists());
    assert!(rift.join("store-bb.db").exists());

    drop(held);
    assert_eq!(current.sweep()?.deleted(), ["bb"]);
    assert!(rift.join("store-cc.db").exists());
    assert!(current.sweep()?.deleted().is_empty());
    Ok(())
}

#[test]
fn an_opener_whose_lock_file_was_replaced_locks_the_new_file() -> TestResult {
    let folder = tempfile::tempdir()?;
    let path = folder.path().join("store-aa.live.lock");
    let mut replaced = false;

    let locked = lock_live_checked(&path, &mut |attempt| {
        if attempt == 0 {
            std::fs::remove_file(&path).expect("the sweeper deletes the lock file");
            std::fs::write(&path, b"").expect("a new opener creates it again");
            replaced = true;
        }
    });

    assert!(replaced);
    let locked = locked?;
    let exclusive = crate::lock::open_lock(&path)?;
    assert!(
        exclusive.try_lock().is_err(),
        "the shared lock is on the file the path names now"
    );
    drop(locked);
    Ok(())
}

#[test]
fn an_opener_refuses_a_lock_file_replaced_on_every_attempt() -> TestResult {
    let folder = tempfile::tempdir()?;
    let path = folder.path().join("store-aa.live.lock");
    let mut attempts = 0;

    let refused = lock_live_checked(&path, &mut |_| {
        attempts += 1;
        std::fs::remove_file(&path).expect("the sweeper deletes the lock file");
        std::fs::write(&path, b"").expect("a new opener creates it again");
    })
    .expect_err("the path never names the locked file");

    assert_eq!(attempts, LIVE_LOCK_ATTEMPTS_MAX);
    assert_eq!(refused.slug(), errors::history_store::lock_unstable::SLUG);
    assert!(
        refused
            .context()
            .any(|(key, value)| key == "attempts" && value == LIVE_LOCK_ATTEMPTS_MAX.to_string())
    );
    assert!(
        std::error::Error::source(&refused).is_none(),
        "no filesystem call failed, so no cause rides the chain"
    );
    let rendered = refused.to_string();
    assert!(
        rendered.contains("history store live lock changed"),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!("during {LIVE_LOCK_ATTEMPTS_MAX} attempts")),
        "{rendered}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_read_only_common_git_directory_refuses_the_folder_without_a_fallback() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    let folder = tempfile::tempdir()?;
    std::fs::set_permissions(folder.path(), std::fs::Permissions::from_mode(0o555))?;
    let opened = HistoryStore::open(&StoreLocation::new(folder.path(), "aa"));
    std::fs::set_permissions(folder.path(), std::fs::Permissions::from_mode(0o755))?;

    let refused = opened.expect_err("the folder refuses the store");
    let cause = std::error::Error::source(&refused)
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .ok_or("the refusal is the folder's")?;
    assert_eq!(cause.kind(), std::io::ErrorKind::PermissionDenied);
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_read_only_common_git_directory_keeps_the_store_in_the_worktree() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    let checkout = tempfile::tempdir()?;
    let git = checkout.path().join(".git");
    let worktree_state = checkout.path().join(".rift");
    std::fs::create_dir_all(&git)?;
    std::fs::create_dir_all(&worktree_state)?;
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o555))?;
    let location = StoreLocation::new(&git, "aa").or_worktree(&worktree_state);
    let opened = HistoryStore::open(&location);
    let second = HistoryStore::open(&location);
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755))?;
    let (store, second) = (opened?, second?);

    let fallback = store
        .worktree_fallback()
        .ok_or("the store names the refusal")?;
    assert_eq!(fallback.refused(), git.join(STORE_FOLDER_NAME));
    let cause = std::error::Error::source(fallback.cause())
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .ok_or("the refusal is the folder's")?;
    assert_eq!(cause.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(store.location().folder(), worktree_state);
    assert!(worktree_state.join("store-aa.db").exists());
    assert!(!git.join(STORE_FOLDER_NAME).exists());

    let mut filler = store.filler()?.ok_or("the filler takes the lock")?;
    filler.write_batch(&[commit("c1", None, 10, "Add lexical search")])?;
    assert_eq!(
        second.reader().connect()?.search_messages("lexical", 10)?,
        ["c1"]
    );
    assert!(second.worktree_fallback().is_some());
    assert!(store.sweep()?.deleted().is_empty());
    Ok(())
}

#[test]
fn a_folder_refusal_other_than_access_keeps_the_refusal_without_a_fallback() -> TestResult {
    let checkout = tempfile::tempdir()?;
    // A linked worktree's `.git` is a file, so no folder can be created below it.
    let git = checkout.path().join(".git");
    std::fs::write(&git, b"gitdir: elsewhere\n")?;
    let worktree_state = checkout.path().join(".rift");
    let location = StoreLocation::new(&git, "aa").or_worktree(&worktree_state);

    let refused = HistoryStore::open(&location).expect_err("no folder is created below a file");

    let cause = std::error::Error::source(&refused)
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .ok_or("the refusal is the folder's")?;
    assert_ne!(cause.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        std::error::Error::source(&refused).is_some(),
        "the filesystem's report rides the cause chain"
    );
    assert!(
        !worktree_state.exists(),
        "only a refusal for want of write access moves the store"
    );
    Ok(())
}

#[test]
fn sweep_reports_a_live_lock_it_cannot_open_and_sweeps_the_rest() -> TestResult {
    let folder = tempfile::tempdir()?;
    let rift = folder.path().join(STORE_FOLDER_NAME);
    drop(filled(folder.path(), "aa")?);
    // A folder where the lock file goes opens as no file on any platform.
    std::fs::create_dir_all(rift.join("store-zz.live.lock"))?;
    let current = HistoryStore::open(&StoreLocation::new(folder.path(), "cc"))?;

    let swept = current.sweep()?;

    assert_eq!(swept.deleted(), ["aa"], "the rest of the folder is swept");
    let [failure] = swept.failures() else {
        panic!("one lock the sweep cannot open: {:?}", swept.failures());
    };
    assert!(std::error::Error::source(failure).is_some());
    let rendered = failure.to_string();
    assert!(rendered.contains("open swept live lock"), "{rendered}");
    assert!(rendered.contains("store-zz.live.lock"), "{rendered}");
    assert!(rift.join("store-zz.live.lock").is_dir());
    Ok(())
}

#[test]
fn sweep_reports_a_file_it_cannot_delete_and_keeps_the_revisions_live_lock() -> TestResult {
    let folder = tempfile::tempdir()?;
    let rift = folder.path().join(STORE_FOLDER_NAME);
    std::fs::create_dir_all(rift.join("store-aa.db"))?;
    std::fs::write(rift.join("store-aa.db/held"), b"")?;
    std::fs::write(rift.join("store-aa.live.lock"), b"")?;
    let current = HistoryStore::open(&StoreLocation::new(folder.path(), "cc"))?;

    let swept = current.sweep()?;

    assert!(swept.deleted().is_empty());
    assert_eq!(swept.failures().len(), 1, "one refused deletion");
    let failure = &swept.failures()[0];
    assert!(std::error::Error::source(failure).is_some());
    let rendered = failure.to_string();
    assert!(rendered.contains("delete swept store file"), "{rendered}");
    assert!(
        rift.join("store-aa.live.lock").exists(),
        "the live lock stays while another file of the revision does"
    );
    assert_eq!(
        current.sweep()?.failures().len(),
        1,
        "a later sweep finds the revision again"
    );
    Ok(())
}

#[test]
fn a_writable_common_git_directory_keeps_the_store_there() -> TestResult {
    let checkout = tempfile::tempdir()?;
    let worktree_state = checkout.path().join(".rift");
    let location =
        StoreLocation::new(&checkout.path().join(".git"), "aa").or_worktree(&worktree_state);

    let store = HistoryStore::open(&location)?;

    assert!(store.worktree_fallback().is_none());
    assert_eq!(store.location().revision(), "aa");
    assert!(checkout.path().join(".git/.rift/store-aa.db").exists());
    assert!(!worktree_state.exists());
    Ok(())
}

#[test]
fn a_store_error_names_its_operation_and_the_drivers_text() -> TestResult {
    let folder = tempfile::tempdir()?;
    let store = filled(folder.path(), "aa")?;
    let reads = store.reader().connect()?;

    let refused = reads
        .search_messages("\"unbalanced", 10)
        .expect_err("a malformed full-text query refuses");

    let rendered = refused.to_string();
    assert!(rendered.contains("search commit messages"), "{rendered}");
    assert!(
        std::error::Error::source(&refused).is_some(),
        "the driver's error rides the cause chain"
    );
    Ok(())
}

/// The count of the histogram series `name` with `labels` that `recorder` holds.
fn recorded(recorder: &rift_tracing::ScopedRecorder, name: &str, labels: &[(&str, &str)]) -> u64 {
    match recorder
        .metrics()
        .find(name, labels)
        .map(rift_tracing::MetricSeries::value)
    {
        Some(rift_tracing::SeriesValue::Buckets { count, .. }) => *count,
        _ => 0,
    }
}

/// The fields of every closed `lock.wait` span `records` hold, parsed.
fn closed_waits(
    records: &[rift_tracing::LogRecord],
) -> Result<Vec<serde_json::Value>, serde_json::Error> {
    records
        .iter()
        .filter(|record| {
            record.message() == "lock.wait" && record.fields().contains("\"span\":\"closed\"")
        })
        .map(|record| serde_json::from_str(record.fields()))
        .collect()
}

const SHARED_LIVE: [(&str, &str); 2] = [("lock.name", "history.live"), ("lock.mode", "shared")];

#[test]
fn an_open_store_records_its_live_lock_wait_and_holds_it_until_dropped() -> TestResult {
    let folder = tempfile::tempdir()?;
    let (recorder, _drain) = rift_tracing::ScopedRecorder::builder().install()?;
    let store = HistoryStore::open(&StoreLocation::new(folder.path(), "aa"))?;

    assert_eq!(
        recorded(&recorder, "lock.wait.duration", &SHARED_LIVE),
        1,
        "one acquisition"
    );
    assert_eq!(
        recorded(&recorder, "lock.held.duration", &SHARED_LIVE),
        0,
        "the store still holds the lock"
    );
    drop(store);
    assert_eq!(recorded(&recorder, "lock.held.duration", &SHARED_LIVE), 1);
    Ok(())
}

#[test]
fn a_refused_filler_records_the_refusal_and_names_the_holder() -> TestResult {
    let folder = tempfile::tempdir()?;
    let location = StoreLocation::new(folder.path(), "aa");
    let first = HistoryStore::open(&location)?;
    let second = HistoryStore::open(&location)?;
    let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
    let filler = rift_tracing::traced!(component = "history", operation = "history.fill", {
        first.filler()
    })?
    .ok_or("the first filler takes the lock")?;

    assert!(second.filler()?.is_none(), "one filler at a time");
    let refused = [
        ("lock.name", "history.fill"),
        ("lock.mode", "exclusive"),
        ("error.type", "refused"),
    ];
    assert_eq!(recorded(&recorder, "lock.wait.duration", &refused), 1);
    drop(filler);
    let held = [("lock.name", "history.fill"), ("lock.mode", "exclusive")];
    assert_eq!(recorded(&recorder, "lock.held.duration", &held), 1);
    drop(recorder);

    let waits = closed_waits(&drain.queued_records())?;
    assert_eq!(waits.len(), 1, "only the refused attempt opens a wait span");
    assert_eq!(waits[0]["lock.name"], "history.fill");
    assert_eq!(waits[0]["outcome"], "refused");
    assert_eq!(
        waits[0]["holder"], "history.fill",
        "the operation that took the lock holds it"
    );
    Ok(())
}

/// The holds a server keeps while it runs, its shared live lock and its fill lock, are
/// listed lifelong in the table of operations in flight, and neither keeps the operation
/// that took it in flight.
#[test]
fn a_servers_live_and_fill_holds_are_listed_lifelong() -> TestResult {
    let folder = tempfile::tempdir()?;
    let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder().install()?;
    let store = rift_tracing::traced!(component = "history", operation = "history.open", {
        HistoryStore::open(&StoreLocation::new(folder.path(), "aa"))
    })?;
    let filler = rift_tracing::traced!(component = "history", operation = "history.fill", {
        store.filler()
    })?
    .ok_or("the first filler takes the lock")?;
    rift_tracing::publish_in_flight("stop");
    drop(filler);
    drop(store);
    drop(recorder);

    let records = drain.queued_records();
    let published = records
        .iter()
        .find(|record| record.message() == "operations in flight")
        .ok_or("the table was published")?;
    let fields: serde_json::Value = serde_json::from_str(published.fields())?;
    let listed: Vec<serde_json::Value> =
        serde_json::from_str(fields["operations"].as_str().ok_or("operations is text")?)?;
    assert_eq!(listed.len(), 2, "the two holds alone: {listed:?}");
    for (lock, parent) in [
        ("history.live", "history.open"),
        ("history.fill", "history.fill"),
    ] {
        let hold = listed
            .iter()
            .find(|entry| entry["lock.name"] == lock)
            .ok_or(lock)?;
        assert_eq!(hold["kind"], "held", "{lock}");
        assert_eq!(hold["lifelong"], true, "{lock}");
        assert_eq!(hold["parent"], parent, "{lock}");
    }
    Ok(())
}

#[test]
fn a_sweep_records_the_live_lock_it_skips_and_the_one_it_takes() -> TestResult {
    let folder = tempfile::tempdir()?;
    let held = HistoryStore::open(&StoreLocation::new(folder.path(), "bb"))?;
    drop(HistoryStore::open(&StoreLocation::new(
        folder.path(),
        "aa",
    ))?);
    let current = HistoryStore::open(&StoreLocation::new(folder.path(), "cc"))?;
    let (recorder, _drain) = rift_tracing::ScopedRecorder::builder().install()?;

    assert_eq!(current.sweep()?.deleted(), ["aa"]);

    let exclusive = [("lock.name", "history.live"), ("lock.mode", "exclusive")];
    let skipped = [
        ("lock.name", "history.live"),
        ("lock.mode", "exclusive"),
        ("error.type", "refused"),
    ];
    assert_eq!(
        recorded(&recorder, "lock.wait.duration", &skipped),
        1,
        "bb is held"
    );
    assert_eq!(
        recorded(&recorder, "lock.wait.duration", &exclusive),
        1,
        "aa is taken"
    );
    assert_eq!(recorded(&recorder, "lock.held.duration", &exclusive), 1);
    drop(held);
    Ok(())
}

#[test]
fn a_written_batch_records_its_write_lock_wait_commit_and_transaction() -> TestResult {
    let folder = tempfile::tempdir()?;
    let store = HistoryStore::open(&StoreLocation::new(folder.path(), "aa"))?;
    let mut filler = store.filler()?.ok_or("no other filler runs")?;
    let (recorder, _drain) = rift_tracing::ScopedRecorder::builder().install()?;

    filler.write_batch(&[commit("c1", None, 10, "Add lexical search")])?;
    assert_eq!(filler.trim(&BTreeSet::new())?, 1);

    let history = [("db.namespace", "history")];
    assert_eq!(
        recorded(&recorder, "sqlite.write_lock.wait.duration", &history),
        2
    );
    assert_eq!(recorded(&recorder, "sqlite.commit.duration", &history), 2);
    let committed = [
        ("db.namespace", "history"),
        ("sqlite.transaction.result", "commit"),
    ];
    assert_eq!(
        recorded(&recorder, "sqlite.transaction.duration", &committed),
        2
    );
    Ok(())
}

#[test]
fn a_failed_batch_records_a_rolled_back_transaction() -> TestResult {
    let folder = tempfile::tempdir()?;
    let store = HistoryStore::open(&StoreLocation::new(folder.path(), "aa"))?;
    let mut filler = store.filler()?.ok_or("no other filler runs")?;
    filler
        .connection()
        .execute_batch("DROP TABLE changed_paths")?;
    let (recorder, _drain) = rift_tracing::ScopedRecorder::builder().install()?;

    filler
        .write_batch(&[commit("c1", None, 10, "Add lexical search")])
        .expect_err("a batch whose table is gone writes nothing");

    let rolled_back = [
        ("db.namespace", "history"),
        ("sqlite.transaction.result", "rollback"),
    ];
    assert_eq!(
        recorded(&recorder, "sqlite.transaction.duration", &rolled_back),
        1
    );
    let history = [("db.namespace", "history")];
    assert_eq!(recorded(&recorder, "sqlite.commit.duration", &history), 0);
    Ok(())
}

#[test]
fn a_batch_refused_the_write_lock_records_the_busy_code() -> TestResult {
    let folder = tempfile::tempdir()?;
    let store = HistoryStore::open(&StoreLocation::new(folder.path(), "aa"))?;
    let mut filler = store.filler()?.ok_or("no other filler runs")?;
    let other = rusqlite::Connection::open(store.location().database())?;
    other.execute_batch("BEGIN IMMEDIATE")?;
    let (recorder, _drain) = rift_tracing::ScopedRecorder::builder().install()?;

    filler
        .write_batch(&[commit("c1", None, 10, "Add lexical search")])
        .expect_err("another connection keeps the write lock past the busy timeout");
    other.execute_batch("ROLLBACK")?;

    let busy = [("db.namespace", "history"), ("error.type", "5")];
    assert_eq!(
        recorded(&recorder, "sqlite.write_lock.wait.duration", &busy),
        1
    );
    assert!(
        recorder
            .metrics()
            .series()
            .iter()
            .all(|series| series.name() != "sqlite.transaction.duration"),
        "no transaction began"
    );
    Ok(())
}
