use std::collections::BTreeSet;
use std::error::Error;
use std::path::Path;

use rift_protocol::read::{CommitAuthor, SymbolVersionKind};

use crate::lock::{LIVE_LOCK_ATTEMPTS_MAX, lock_live_checked};
use crate::{
    ChangedPath, CommitRecord, DeclarationChange, HistoryStore, MovedDeclaration, RenamedPath,
    StoreFault, StoreLocation,
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
    assert!(matches!(refused.fault(), StoreFault::Database { .. }));
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
    let rift = folder.path().join("rift");
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
    assert!(matches!(
        refused.fault(),
        StoreFault::LockUnstable { attempts, .. } if *attempts == LIVE_LOCK_ATTEMPTS_MAX
    ));
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
    let cause = refused
        .fault()
        .folder_cause()
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
    assert_eq!(fallback.refused(), git.join("rift"));
    let cause = fallback
        .cause()
        .fault()
        .folder_cause()
        .ok_or("the refusal is the folder's")?;
    assert_eq!(cause.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(store.location().folder(), worktree_state);
    assert!(worktree_state.join("store-aa.db").exists());
    assert!(!git.join("rift").exists());

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
fn a_writable_common_git_directory_keeps_the_store_there() -> TestResult {
    let checkout = tempfile::tempdir()?;
    let worktree_state = checkout.path().join(".rift");
    let location =
        StoreLocation::new(&checkout.path().join(".git"), "aa").or_worktree(&worktree_state);

    let store = HistoryStore::open(&location)?;

    assert!(store.worktree_fallback().is_none());
    assert_eq!(store.location().revision(), "aa");
    assert!(checkout.path().join(".git/rift/store-aa.db").exists());
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
