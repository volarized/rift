use std::fs;
use std::path::Path;

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
fn marking_driver(root: &Path) -> (std::path::PathBuf, String) {
    let marker = root.join(".git/driver-ran");
    let command = format!("sh -c 'touch {}; cat'", marker.display());
    (marker, command)
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

    let (paths, _) = changed(root, "HEAD", &[], 512);
    let repository = Repository::open(root).expect("repository");
    let head = repository.resolve("HEAD").expect("head");
    let files = repository
        .tree_files(&head, &|path| path.ends_with(".bin"), 16)
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
    assert!(
        !root.join(".git/lfs").exists(),
        "no read stores an object under .git/lfs"
    );
}

#[test]
fn the_base_side_converts_built_in_filters_and_runs_no_driver() {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    init(root);
    let marker = root.join(".git/driver-ran");
    let command = format!("sh -c 'touch {}; tr a-z A-Z'", marker.display());
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
