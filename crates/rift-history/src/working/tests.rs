use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use super::{WorkingForm, blob_id_of_length, pointer_size};
use crate::Repository;
use crate::fixture::{commit_all, git, init};

/// git-lfs 3.8.0's pointers for `binary payload of unedited\n` (27 bytes)
/// and `binary payload of edited\n` (25 bytes).
const UNEDITED_POINTER: &str = "version https://git-lfs.github.com/spec/v1\noid sha256:84479338e39d2628d302e12c4dfc313cb10e7c1c2aa907b3a8b5a1b24fb61bf2\nsize 27\n";
const EDITED_POINTER: &str = "version https://git-lfs.github.com/spec/v1\noid sha256:b0871cd408e4945b7e14b30515c92ba77c33e44e6d35f3ed2529d9415609b3e1\nsize 25\n";

fn write(root: &Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        fs::write(root.join(path), text).expect("fixture file");
    }
}

fn changed(root: &Path, base: &str, published: &[&str], paths_max: usize) -> (Vec<String>, bool) {
    let repository = Repository::open(root).expect("repository");
    let base = repository.resolve(base).expect("base");
    let changed = repository
        .changed_working_files(&base, published, &|_| true, paths_max)
        .expect("working changes");
    (changed.paths().to_vec(), changed.is_truncated())
}

/// A driver command that leaves a marker file behind whenever git starts it,
/// so a test proves no read ran it.
fn marking_driver(root: &Path) -> (PathBuf, String) {
    let marker = root.join(".git/driver-ran");
    let command = format!("sh -c 'touch \"{}\"; cat'", shell_path(&marker));
    (marker, command)
}

/// `path` as the shell git starts a driver through reads it. That shell takes `\` for an
/// escape character, so a Windows path spells each separator `/`, the form Git for
/// Windows' shell reads as the same path.
fn shell_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// Every path below `directory`, relative to it; empty when `directory` does not exist.
///
/// A host with git-lfs installed configures its filter for every repository, so the
/// fixture's own `git add` may already have run it and left `.git/lfs` behind. A read
/// proves it stores nothing there by leaving this listing as it found it.
fn listing(directory: &Path) -> BTreeSet<PathBuf> {
    let mut entries = BTreeSet::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(folder) = pending.pop() {
        let Ok(children) = fs::read_dir(&folder) else {
            continue;
        };
        for child in children {
            let path = child.expect("a listed entry").path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            let relative = path
                .strip_prefix(directory)
                .expect("below the listed folder");
            entries.insert(relative.to_path_buf());
        }
    }
    entries
}

#[test]
fn working_changes_list_every_path_whose_bytes_differ_from_the_base() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    init(root);
    for name in [
        "staged", "unstaged", "reverted", "deleted", "renamed", "kept", "later",
    ] {
        write(
            root,
            &[(&format!("{name}.rs"), &format!("pub fn {name}() {{}}\n"))],
        );
    }
    commit_all(root, "base");
    write(root, &[("later.rs", "pub fn later(flag: bool) {}\n")]);
    commit_all(root, "head");
    write(root, &[("staged.rs", "pub fn staged(flag: bool) {}\n")]);
    git(root, &["add", "staged.rs"]);
    write(root, &[("unstaged.rs", "pub fn unstaged(flag: bool) {}\n")]);
    write(root, &[("reverted.rs", "pub fn reverted(flag: bool) {}\n")]);
    git(root, &["add", "reverted.rs"]);
    write(root, &[("reverted.rs", "pub fn reverted() {}\n")]);
    write(root, &[("added.rs", "pub fn added() {}\n")]);
    git(root, &["add", "added.rs"]);
    fs::remove_file(root.join("deleted.rs")).expect("delete");
    git(root, &["mv", "renamed.rs", "moved.rs"]);
    write(root, &[("untracked.rs", "pub fn untracked() {}\n")]);
    write(root, &[("excluded.rs", "pub fn excluded() {}\n")]);
    write(root, &[(".git/info/exclude", "excluded.rs\n")]);
    let published = ["excluded.rs", "kept.rs", "staged.rs", "untracked.rs"];

    let (paths, truncated) = changed(root, "HEAD~1", &published, 512);

    // `reverted.rs` is staged but its working bytes are the base's again, and
    // `excluded.rs` is served although git's own excludes name it. The staged
    // `git mv` lists the deletion beside the addition.
    let expected = [
        "added.rs",
        "deleted.rs",
        "excluded.rs",
        "later.rs",
        "moved.rs",
        "renamed.rs",
        "staged.rs",
        "unstaged.rs",
        "untracked.rs",
    ];
    assert_eq!(paths, expected);
    assert!(!truncated);
}

#[test]
fn a_file_only_the_configured_excludes_file_hides_is_listed_untracked() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    init(root);
    write(root, &[("kept.rs", "pub fn kept() {}\n")]);
    commit_all(root, "base");
    let excludes = root.join(".git/global-excludes");
    fs::write(&excludes, "scratch.rs\n").expect("excludes file");
    git(
        root,
        &[
            "config",
            "core.excludesFile",
            &excludes.display().to_string(),
        ],
    );
    write(root, &[("scratch.rs", "pub fn scratch() {}\n")]);

    let (paths, _) = changed(root, "HEAD", &["kept.rs", "scratch.rs"], 512);

    assert_eq!(paths, ["scratch.rs"]);
}

#[test]
fn a_file_the_published_index_does_not_hold_is_not_listed_untracked() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    init(root);
    write(root, &[("kept.rs", "pub fn kept() {}\n")]);
    commit_all(root, "base");
    write(root, &[("unserved.rs", "pub fn unserved() {}\n")]);

    let (paths, _) = changed(root, "HEAD", &["kept.rs"], 512);

    assert!(paths.is_empty(), "{paths:?}");
}

#[test]
fn working_changes_stop_at_the_path_bound() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    init(root);
    write(root, &[("kept.rs", "pub fn kept() {}\n")]);
    commit_all(root, "base");
    for index in 0..5 {
        write(
            root,
            &[(&format!("added{index}.rs"), "pub fn added() {}\n")],
        );
    }
    let published = [
        "added0.rs",
        "added1.rs",
        "added2.rs",
        "added3.rs",
        "added4.rs",
    ];

    let (paths, truncated) = changed(root, "HEAD", &published, 3);
    assert_eq!(paths, ["added0.rs", "added1.rs", "added2.rs"]);
    assert!(truncated);

    let (paths, truncated) = changed(root, "HEAD", &published, 5);
    assert_eq!(paths.len(), 5);
    assert!(!truncated, "exactly the bound is not a cut");
}

#[test]
fn an_lfs_file_answers_changed_only_when_its_bytes_changed() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    init(root);
    write(
        root,
        &[
            ("unedited.bin", UNEDITED_POINTER),
            ("edited.bin", EDITED_POINTER),
            ("resized.bin", EDITED_POINTER),
            ("pointer.bin", UNEDITED_POINTER),
        ],
    );
    commit_all(root, "pointers");
    write(root, &[(".gitattributes", "*.bin filter=lfs -text\n")]);
    commit_all(root, "attributes");
    let repository = Repository::open(root).expect("repository");
    let head = repository.resolve("HEAD").expect("head");
    let files = repository
        .tree_files(&head, &|path| path == "unedited.bin", 16)
        .expect("files");
    assert_eq!(
        files[0].blob_id(),
        "f66a77b5ed3d804ab2b035512810eb4b8966f346"
    );
    // The working files as a smudge leaves them; `pointer.bin` stays a pointer.
    write(
        root,
        &[
            ("unedited.bin", "binary payload of unedited\n"),
            ("edited.bin", "binary payload of EDITED\n"),
            ("resized.bin", "binary payload, now longer\n"),
        ],
    );

    let (paths, _) = changed(root, "HEAD", &[], 512);

    assert_eq!(paths, ["edited.bin", "resized.bin"]);
}

#[test]
fn a_comparison_over_lfs_paths_starts_no_driver_and_stores_nothing() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    init(root);
    write(
        root,
        &[
            ("edited.bin", EDITED_POINTER),
            ("unedited.bin", UNEDITED_POINTER),
        ],
    );
    commit_all(root, "pointers");
    write(root, &[(".gitattributes", "*.bin filter=lfs -text\n")]);
    commit_all(root, "attributes");
    let (marker, command) = marking_driver(root);
    for key in [
        "filter.lfs.clean",
        "filter.lfs.smudge",
        "filter.lfs.process",
    ] {
        git(root, &["config", key, &command]);
    }
    git(root, &["config", "filter.lfs.required", "true"]);
    write(
        root,
        &[
            ("edited.bin", "binary payload of EDITED\n"),
            ("unedited.bin", "binary payload of unedited\n"),
        ],
    );

    let stored = listing(&root.join(".git/lfs"));

    let (paths, _) = changed(root, "HEAD", &[], 512);
    let repository = Repository::open(root).expect("repository");
    let head = repository.resolve("HEAD").expect("head");
    let files = repository
        .tree_files(
            &head,
            &|path| {
                Path::new(path)
                    .extension()
                    .is_some_and(|extension| extension == "bin")
            },
            16,
        )
        .expect("files");
    let mut forms = repository.working_forms().expect("working forms");
    for file in &files {
        assert_eq!(
            forms.form(file, 1024).expect("working form"),
            WorkingForm::Filtered {
                driver: "lfs".to_owned()
            }
        );
    }

    assert_eq!(paths, ["edited.bin"]);
    assert!(!marker.exists(), "no read starts the lfs driver");
    assert_eq!(
        listing(&root.join(".git/lfs")),
        stored,
        "no read stores an object under .git/lfs"
    );
}

#[test]
fn the_base_side_converts_built_in_filters_and_runs_no_driver() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    init(root);
    let marker = root.join(".git/driver-ran");
    let command = format!("sh -c 'touch \"{}\"; tr a-z A-Z'", shell_path(&marker));
    git(root, &["config", "filter.upper.clean", &command]);
    git(root, &["config", "filter.upper.smudge", &command]);
    write(
        root,
        &[
            (
                ".gitattributes",
                "*.rs text eol=crlf\n*.up filter=upper\n*.u16 working-tree-encoding=UTF-16LE\n",
            ),
            ("lib.rs", "pub fn beacon() {\n}\n"),
            ("notes.up", "text\n"),
        ],
    );
    let utf16: Vec<u8> = "text\n".encode_utf16().flat_map(u16::to_le_bytes).collect();
    fs::write(root.join("notes.u16"), utf16).expect("fixture file");
    commit_all(root, "base");
    fs::remove_file(&marker).expect("git add ran the clean command");
    write(root, &[("notes.up", "text\n")]);
    let repository = Repository::open(root).expect("repository");
    let head = repository.resolve("HEAD").expect("head");

    let (paths, _) = changed(root, "HEAD", &[], 512);
    let files = repository
        .tree_files(&head, &|path| path != ".gitattributes", 16)
        .expect("files");
    let mut converter = repository.working_forms().expect("working forms");
    let forms: Vec<WorkingForm> = files
        .iter()
        .map(|file| converter.form(file, 1024).expect("working form"))
        .collect();

    assert_eq!(paths, ["notes.up"]);
    assert_eq!(
        forms,
        [
            WorkingForm::Converted(b"pub fn beacon() {\r\n}\r\n".to_vec()),
            WorkingForm::Encoded {
                encoding: "UTF-16LE".to_owned()
            },
            WorkingForm::Filtered {
                driver: "upper".to_owned()
            },
        ]
    );
    assert!(!marker.exists(), "no filter command runs during the read");
}

#[test]
fn a_working_form_past_the_byte_bound_refuses_the_blob() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    init(root);
    write(root, &[("lib.rs", "pub fn beacon() {}\n")]);
    commit_all(root, "base");
    let repository = Repository::open(root).expect("repository");
    let head = repository.resolve("HEAD").expect("head");
    let files = repository.tree_files(&head, &|_| true, 16).expect("files");
    let mut converter = repository.working_forms().expect("working forms");

    let error = converter
        .form(&files[0], 4)
        .expect_err("the blob is past 4 bytes");

    assert!(matches!(
        error.fault(),
        crate::HistoryFault::BlobTooLarge { bytes_max: 4, .. }
    ));
    let rendered = format!("{converter:?}");
    assert!(rendered.starts_with("WorkingForms"), "{rendered}");
}

#[test]
fn a_staged_pointer_edit_reverted_in_the_working_tree_answers_unchanged() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    init(root);
    write(root, &[("pointer.bin", EDITED_POINTER)]);
    commit_all(root, "edited pointer");
    write(root, &[("pointer.bin", UNEDITED_POINTER)]);
    commit_all(root, "unedited pointer");
    write(root, &[(".gitattributes", "*.bin filter=lfs -text\n")]);
    commit_all(root, "attributes");
    let edited = gix::objs::compute_hash(
        gix::hash::Kind::Sha1,
        gix::objs::Kind::Blob,
        EDITED_POINTER.as_bytes(),
    )
    .expect("hash");
    // The index names the edited pointer while the working file holds the base's,
    // so the index-to-working-tree pass reports the path with the base's bytes.
    let entry = format!("100644,{edited},pointer.bin");
    git(root, &["update-index", "--cacheinfo", &entry]);

    let (paths, _) = changed(root, "HEAD", &[], 512);

    assert!(paths.is_empty(), "{paths:?}");
}

/// The git-lfs spec v1 pointer for `content`.
fn pointer_for(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    let size = content.len();
    format!("version https://git-lfs.github.com/spec/v1\noid sha256:{digest:x}\nsize {size}\n")
}

#[test]
fn an_lfs_file_longer_than_any_pointer_matches_its_base_by_digest() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    init(root);
    let unedited = "a payload line past the pointer bytes\n".repeat(40);
    let edited = unedited.replacen("payload", "PAYLOAD", 1);
    let pointer = pointer_for(&unedited);
    write(
        root,
        &[
            ("unedited.bin", pointer.as_str()),
            ("edited.bin", pointer.as_str()),
        ],
    );
    commit_all(root, "pointers");
    write(root, &[(".gitattributes", "*.bin filter=lfs -text\n")]);
    commit_all(root, "attributes");
    write(
        root,
        &[
            ("unedited.bin", unedited.as_str()),
            ("edited.bin", edited.as_str()),
        ],
    );

    let (paths, _) = changed(root, "HEAD", &[], 512);

    assert!(unedited.len() > 1024 && edited.len() == unedited.len());
    assert_eq!(paths, ["edited.bin"]);
}

#[test]
fn an_lfs_path_whose_base_holds_its_content_answers_changed_when_edited() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    init(root);
    let content = "a payload committed before the path was tracked by lfs\n".repeat(30);
    write(root, &[("kept.bin", content.as_str())]);
    commit_all(root, "content");
    // The attribute lives outside the tree, so no commit runs a clean filter over the
    // content: the base blob is the content itself, longer than any pointer.
    fs::create_dir_all(root.join(".git/info")).expect("info folder");
    fs::write(
        root.join(".git/info/attributes"),
        "*.bin filter=lfs -text\n",
    )
    .expect("attributes");
    let edited = content.replacen("payload", "PAYLOAD", 1);
    write(root, &[("kept.bin", edited.as_str())]);

    let (paths, _) = changed(root, "HEAD", &[], 512);

    assert!(content.len() > 1024);
    assert_eq!(paths, ["kept.bin"]);
}

#[test]
fn a_workspace_below_the_repository_root_reads_its_own_paths() {
    for (autocrlf, expected) in [
        ("false", b"pub fn beacon() {}\n".as_slice()),
        ("true", b"pub fn beacon() {}\r\n".as_slice()),
    ] {
        let directory = tempfile::tempdir().expect("temp dir");
        let root = directory.path();
        init(root);
        git(root, &["config", "core.autocrlf", autocrlf]);
        fs::create_dir_all(root.join("sub")).expect("workspace folder");
        write(
            root,
            &[
                ("sub/lib.rs", "pub fn beacon() {}\n"),
                ("other.rs", "pub fn other() {}\n"),
            ],
        );
        commit_all(root, "base");
        write(
            root,
            &[
                ("sub/lib.rs", "pub fn beacon(flag: bool) {}\n"),
                ("sub/new.rs", "pub fn new() {}\n"),
                ("other.rs", "pub fn other(flag: bool) {}\n"),
            ],
        );
        let repository = Repository::open(&root.join("sub")).expect("repository");
        let base = repository.resolve("HEAD").expect("base");

        let changed = repository
            .changed_working_files(&base, &["lib.rs", "new.rs"], &|_| true, 512)
            .expect("working changes");
        let files = repository.tree_files(&base, &|_| true, 16).expect("files");
        let mut forms = repository.working_forms().expect("working forms");
        let form = forms.form(&files[0], 1024).expect("working form");

        assert_eq!(changed.paths(), ["lib.rs", "new.rs"]);
        assert_eq!(files.len(), 1, "the workspace holds its own files alone");
        assert_eq!(
            form,
            WorkingForm::Converted(expected.to_vec()),
            "core.autocrlf={autocrlf}"
        );
    }
}

#[test]
fn blob_id_of_length_hashes_only_a_stream_of_exactly_that_length() {
    let bytes = b"pub fn beacon() {}\n";
    let expected =
        gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::objs::Kind::Blob, bytes).expect("hash");
    let length = bytes.len() as u64;

    let exact = blob_id_of_length(&bytes[..], length, gix::hash::Kind::Sha1).expect("read");
    let longer = blob_id_of_length(&bytes[..], length - 1, gix::hash::Kind::Sha1).expect("read");
    let shorter = blob_id_of_length(&bytes[..], length + 1, gix::hash::Kind::Sha1).expect("read");

    assert_eq!(exact, Some(expected));
    assert_eq!(longer, None);
    assert_eq!(shorter, None);
}

#[test]
fn pointer_size_reads_the_size_line_alone() {
    assert_eq!(pointer_size(UNEDITED_POINTER.as_bytes()), Some(27));
    assert_eq!(pointer_size(b"version x\nsize many\n"), None);
    assert_eq!(pointer_size(b"not a pointer"), None);
}
