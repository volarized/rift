use std::fs;
use std::path::Path;

use crate::fixture::{commit_all, git, init};
use crate::{HistoryFault, Repository};

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("fixture folder");
    }
    fs::write(path, text).expect("fixture file");
}

/// Three commits on `main`: `lib.rs` introduced, edited, then `moved.rs`
/// added beside a deleted `gone.rs`.
fn three_commits() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    init(root);
    write(root, "lib.rs", "pub fn beacon() {}\n");
    write(root, "gone.rs", "pub fn gone() {}\n");
    commit_all(root, "introduce beacon");
    write(root, "lib.rs", "pub fn beacon(flag: bool) {}\n");
    commit_all(root, "grow beacon\n\nThe body names the reason.");
    fs::remove_file(root.join("gone.rs")).expect("delete");
    write(root, "sub/moved.rs", "pub fn moved() {}\n");
    commit_all(root, "move things");
    directory
}

fn head_of(repository: &Repository) -> crate::ResolvedRevision {
    repository.resolve("HEAD").expect("head")
}

/// The `(path, has old blob, has new blob)` triple of every changed path.
fn sides(changed: &super::ChangedBlobs) -> Vec<(&str, bool, bool)> {
    changed
        .blobs()
        .iter()
        .map(|blob| {
            (
                blob.path(),
                blob.old_blob().is_some(),
                blob.new_blob().is_some(),
            )
        })
        .collect()
}

#[test]
fn a_first_parent_window_lists_the_newest_commits_with_their_parents() {
    let directory = three_commits();
    let repository = Repository::open(directory.path()).expect("repository");
    let head = head_of(&repository);

    let window = repository.first_parent_window(&head, 10).expect("window");

    assert_eq!(window.len(), 3);
    assert_eq!(window[0].revision(), &head);
    assert_eq!(window[0].parent(), Some(window[1].revision()));
    assert_eq!(window[2].parent(), None, "the root commit has no parent");
    assert!(window.iter().all(|commit| !commit.is_boundary()));

    let bounded = repository.first_parent_window(&head, 2).expect("window");
    assert_eq!(bounded.len(), 2);
    assert_eq!(bounded[1].parent(), Some(window[2].revision()));
    assert!(
        repository
            .first_parent_window(&head, 0)
            .expect("window")
            .is_empty()
    );
}

#[test]
fn a_first_parent_window_stops_at_a_shallow_boundary() {
    let directory = three_commits();
    let repository = Repository::open(directory.path()).expect("repository");
    let head = head_of(&repository);
    let middle = repository.resolve("HEAD~1").expect("middle");
    fs::write(
        directory.path().join(".git/shallow"),
        format!("{}\n", middle.commit_id()),
    )
    .expect("shallow file");
    let repository = Repository::open(directory.path()).expect("repository");

    let window = repository.first_parent_window(&head, 10).expect("window");

    assert_eq!(window.len(), 2);
    assert!(window[1].is_boundary());
    assert_eq!(window[1].parent(), None);
}

#[test]
fn commit_facts_carry_author_time_and_the_full_message() {
    let directory = three_commits();
    let repository = Repository::open(directory.path()).expect("repository");
    let middle = repository.resolve("HEAD~1").expect("middle");

    let facts = repository.commit_facts(&middle).expect("facts");

    assert_eq!(facts.author_name(), "Rift Fixture");
    assert_eq!(facts.author_email(), "fixture@rift.invalid");
    assert_eq!(facts.committed_at(), "2026-01-01T00:00:00+00:00");
    assert_eq!(facts.committed_seconds(), 1_767_225_600);
    assert_eq!(
        facts.message(),
        "grow beacon\n\nThe body names the reason.\n"
    );
}

#[test]
fn changed_blobs_name_each_side_of_every_changed_path() {
    let directory = three_commits();
    let repository = Repository::open(directory.path()).expect("repository");
    let head = head_of(&repository);
    let middle = repository.resolve("HEAD~1").expect("middle");
    let root = repository.resolve("HEAD~2").expect("root");

    let moved = repository
        .changed_blobs(Some(&middle), &head, &|_| true, 16)
        .expect("changes");
    assert_eq!(
        sides(&moved),
        [("gone.rs", true, false), ("sub/moved.rs", false, true)]
    );
    assert!(!moved.is_truncated());

    let edited = repository
        .changed_blobs(Some(&root), &middle, &|_| true, 16)
        .expect("changes");
    let [lib] = edited.blobs() else {
        panic!("one path changed: {edited:?}");
    };
    assert_eq!(lib.path(), "lib.rs");
    assert_ne!(
        lib.old_blob().map(crate::TreeFile::blob_id),
        lib.new_blob().map(crate::TreeFile::blob_id)
    );

    let introduced = repository
        .changed_blobs(None, &root, &|path| path != "gone.rs", 16)
        .expect("changes");
    let paths: Vec<&str> = introduced
        .blobs()
        .iter()
        .map(super::ChangedBlob::path)
        .collect();
    assert_eq!(paths, ["lib.rs"], "a root commit adds every file it holds");
    assert!(introduced.blobs()[0].old_blob().is_none());

    let bounded = repository
        .changed_blobs(Some(&middle), &head, &|_| true, 1)
        .expect("changes");
    assert_eq!(bounded.blobs().len(), 1);
    assert!(bounded.is_truncated());
}

#[test]
fn tagged_commits_peel_annotated_tags_and_refuse_past_the_bound() {
    let directory = three_commits();
    let root = directory.path();
    git(root, &["tag", "v0.0.1", "HEAD~2"]);
    git(root, &["tag", "-a", "v0.0.2", "-m", "release", "HEAD"]);
    git(root, &["tag", "v0.0.3-tree", "HEAD^{tree}"]);
    let repository = Repository::open(root).expect("repository");
    let head = head_of(&repository);

    let tagged = repository.tagged_commits(8).expect("tags");

    let names: Vec<&str> = tagged.iter().map(crate::TaggedCommit::name).collect();
    assert_eq!(
        names,
        ["v0.0.1", "v0.0.2"],
        "a tag naming a tree is left out"
    );
    assert_eq!(tagged[1].revision(), &head);

    let refused = repository
        .tagged_commits(1)
        .expect_err("two tags exceed a bound of one");
    assert!(matches!(
        refused.fault(),
        HistoryFault::TooManyTags { tags_max: 1 }
    ));
    let rendered = refused.to_string();
    assert!(rendered.contains("tags_max 1"), "{rendered}");
}

#[test]
fn changed_blobs_refuse_a_tree_the_object_store_cannot_read() {
    let directory = three_commits();
    let root = directory.path();
    crate::fixture::commit_missing_subtree(root, "refs/heads/broken");
    let repository = Repository::open(root).expect("repository");
    let head = head_of(&repository);
    let broken = repository.resolve("broken").expect("broken resolves");

    let error = repository
        .changed_blobs(Some(&head), &broken, &|_| true, 16)
        .expect_err("an unreadable tree refuses rather than answering part of the listing");

    let rendered = error.to_string();
    assert!(rendered.contains("compare commit trees"), "{rendered}");
}

#[test]
fn changed_blobs_list_a_blob_that_replaced_a_folder_beside_the_folders_files() {
    let directory = three_commits();
    let root = directory.path();
    fs::remove_dir_all(root.join("sub")).expect("delete the folder");
    write(root, "sub", "a file where the folder was\n");
    commit_all(root, "replace the folder");
    let repository = Repository::open(root).expect("repository");
    let head = head_of(&repository);
    let folder = repository.resolve("HEAD~1").expect("the folder's commit");

    let changed = repository
        .changed_blobs(Some(&folder), &head, &|_| true, 16)
        .expect("changes");

    assert_eq!(
        sides(&changed),
        [("sub", false, true), ("sub/moved.rs", true, false)]
    );
}

#[test]
fn a_file_a_symbolic_link_replaced_is_listed_with_the_files_blob_alone() {
    let directory = three_commits();
    let root = directory.path();
    crate::fixture::commit_symlink_in_place(root, "lib.rs", "sub/moved.rs", "refs/heads/main");
    let repository = Repository::open(root).expect("repository");
    let link = head_of(&repository);
    let file = repository.resolve("HEAD~1").expect("the file's commit");

    let replaced = repository
        .changed_blobs(Some(&file), &link, &|_| true, 16)
        .expect("changes");
    let restored = repository
        .changed_blobs(Some(&link), &file, &|_| true, 16)
        .expect("changes");
    let listed = repository
        .changed_files(&file, &link, &|_| true, 16)
        .expect("changes");

    assert_eq!(
        sides(&replaced),
        [("lib.rs", true, false)],
        "the link holds no blob, so the file reads as deleted"
    );
    assert_eq!(sides(&restored), [("lib.rs", false, true)]);
    assert_eq!(listed.paths(), ["lib.rs"]);
}

#[test]
fn changed_blobs_name_a_path_a_malformed_tree_holds_twice_once() {
    let directory = three_commits();
    let root = directory.path();
    crate::fixture::commit_duplicate_path(root, "lib.rs", "refs/heads/duplicated");
    let repository = Repository::open(root).expect("repository");
    let head = head_of(&repository);
    let duplicated = repository
        .resolve("duplicated")
        .expect("duplicated resolves");

    let changed = repository
        .changed_blobs(Some(&head), &duplicated, &|_| true, 16)
        .expect("changes");

    let paths: Vec<&str> = changed
        .blobs()
        .iter()
        .map(super::ChangedBlob::path)
        .collect();
    assert_eq!(paths, ["lib.rs", "sub/moved.rs"]);
    let lib = &changed.blobs()[0];
    assert!(lib.old_blob().is_some() && lib.new_blob().is_some());
    let files = repository
        .changed_files(&head, &duplicated, &|_| true, 16)
        .expect("changes");
    assert_eq!(files.paths(), ["lib.rs", "sub/moved.rs"]);
}

#[test]
fn heads_list_every_live_worktree_git_would_not_prune() {
    let directory = three_commits();
    let root = directory.path();
    let others = tempfile::tempdir().expect("temp dir");
    let linked = others.path().join("linked");
    let pruned = others.path().join("pruned");
    let locked = others.path().join("locked");
    git(
        root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            &linked.display().to_string(),
            "HEAD~1",
        ],
    );
    git(
        root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "pruned",
            &pruned.display().to_string(),
            "HEAD~2",
        ],
    );
    write(&pruned, "pruned.rs", "pub fn pruned() {}\n");
    commit_all(&pruned, "work only the pruned worktree holds");
    let pruned_head = Repository::open(&pruned)
        .expect("pruned")
        .resolve("HEAD")
        .expect("head");
    git(
        root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "locked",
            &locked.display().to_string(),
            "HEAD~2",
        ],
    );
    git(root, &["worktree", "lock", &locked.display().to_string()]);
    fs::remove_dir_all(&pruned).expect("delete a worktree folder");
    fs::remove_dir_all(&locked).expect("delete a locked worktree folder");
    let repository = Repository::open(&linked).expect("repository");

    let heads = repository.live_heads().expect("heads");

    let main = Repository::open(root).expect("main");
    let expected = [
        main.resolve("HEAD~1").expect("linked head"),
        main.resolve("HEAD").expect("main head"),
        main.resolve("HEAD~2").expect("locked head"),
    ];
    assert_eq!(
        heads, expected,
        "the serving worktree first, each head once"
    );
    assert!(
        !heads.contains(&pruned_head),
        "a prunable worktree holds no window"
    );
    let common = |repository: &Repository| {
        fs::canonicalize(repository.common_directory()).expect("common directory")
    };
    assert_eq!(common(&repository), common(&main));
    assert!(common(&main).ends_with(".git"));
}
