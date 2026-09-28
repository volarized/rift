//! A file past `[search.text] max_chunk` or past `[providers.syntax] max_file` is never
//! dropped under the default `split`: its text answers in chunks, and a file the syntax
//! provider cannot parse for its size is held as text alone. Under `skip`, the text index
//! leaves such a file out, and a file past `max_file` leaves the index.

use std::fmt::Write as _;
use std::path::Path;

use rift_core::{LanguageFileSelections, SourceVisibility, TextFileInclusion};
use rift_index::{
    LastCapture, WorkspaceIndex, WorkspaceIndexLimits, WorkspaceIndexWarning,
    capture_digests_with_languages,
};
use rift_protocol::configuration::LargeFileStrategy;
use rift_ranking::DocumentKind;
use rift_syntax::SyntaxLimits;
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// The smallest chunk bound the configuration accepts.
const CHUNK_BYTES_MAX: u64 = 1_024;

/// A syntax source bound small enough that `big.rs` passes it.
const SOURCE_BYTES_MAX: usize = 2_048;

/// A Rust file past both bounds: several kilobytes of one declaration per line.
fn big_source() -> String {
    (0..120).fold(String::new(), |mut source, index| {
        let _ = writeln!(
            source,
            "pub const BEACON_{index:03}: u32 = {index}; // lantern"
        );
        source
    })
}

/// Writes a small parsed file, `big.rs`, and a text file past the chunk bound.
fn tree() -> TestResult<TempDir> {
    let tree = TempDir::new()?;
    std::fs::write(tree.path().join("lib.rs"), "pub fn beacon() {}\n")?;
    std::fs::write(tree.path().join("big.rs"), big_source())?;
    std::fs::write(
        tree.path().join("long.txt"),
        "a line of plain filler text\n".repeat(50),
    )?;
    Ok(tree)
}

fn limits(strategy: LargeFileStrategy) -> TestResult<WorkspaceIndexLimits> {
    Ok(WorkspaceIndexLimits::default()
        .with_syntax(SyntaxLimits::new(SOURCE_BYTES_MAX, 250_000, 512)?)
        .with_large_files(strategy))
}

fn inclusion(strategy: LargeFileStrategy) -> TextFileInclusion {
    TextFileInclusion::new(vec!["**".to_owned()], CHUNK_BYTES_MAX).with_large_files(strategy)
}

fn build(root: &Path, strategy: LargeFileStrategy) -> TestResult<WorkspaceIndex> {
    Ok(WorkspaceIndex::build_with_languages(
        root,
        limits(strategy)?,
        &SourceVisibility::default(),
        &inclusion(strategy),
        &LanguageFileSelections::default(),
    )?)
}

/// The paths the index's text documents belong to, one entry per row.
fn text_rows(index: &WorkspaceIndex) -> Vec<String> {
    index
        .index_documents()
        .iter()
        .filter(|document| document.kind() == DocumentKind::TextFile)
        .filter_map(|document| document.project_path().map(|path| path.as_str().to_owned()))
        .collect()
}

/// Under `split`, a file past `max_file` is held as text the provider does not parse:
/// its text answers in chunk rows, no declaration of it is indexed, and its warning says
/// the text is held. A capture of the tree agrees with the index on every digest.
#[test]
fn split_holds_a_file_past_max_file_as_unparsed_text_in_chunks() -> TestResult {
    let tree = tree()?;
    let index = build(tree.path(), LargeFileStrategy::Split)?;
    let big = rift_core::ProjectPath::new("big.rs")?;
    assert!(
        index.file(&big).is_none(),
        "the provider parsed nothing of big.rs"
    );
    assert!(index.text_file(&big).is_some(), "big.rs is held as text");
    let [warning] = index.warnings() else {
        return Err(format!("one warning names big.rs: {:?}", index.warnings()).into());
    };
    assert!(warning.holds_text(), "{warning:?}");
    assert_eq!(warning.path(), &big);
    let rows = text_rows(&index);
    assert!(
        rows.iter().filter(|path| *path == "big.rs").count() > 1,
        "big.rs answers in chunk rows: {rows:?}"
    );
    assert!(rows.iter().filter(|path| *path == "long.txt").count() > 1);
    assert!(
        index
            .index_documents()
            .iter()
            .all(|document| !document.identity().as_str().contains("BEACON_")),
        "no declaration of big.rs is indexed"
    );
    let (captured, _) = capture_digests_with_languages(
        tree.path(),
        limits(LargeFileStrategy::Split)?,
        &SourceVisibility::default(),
        &inclusion(LargeFileStrategy::Split),
        &LanguageFileSelections::default(),
        &LastCapture::default(),
    )?;
    assert_eq!(
        captured,
        index.digests(),
        "a capture reads the held text as the build did"
    );
    Ok(())
}

/// Under `skip`, a file past `max_chunk` keeps no text row and leaves text search, while a
/// declaration it holds still answers, and a file past `max_file` leaves the index as a
/// file past the per-file byte bound.
#[test]
fn skip_leaves_files_past_the_chunk_bound_out_of_text_search() -> TestResult {
    let tree = tree()?;
    std::fs::write(
        tree.path().join("wide.rs"),
        "pub fn wide() {}\n".to_owned() + &"// filler comment line\n".repeat(60),
    )?;
    let index = build(tree.path(), LargeFileStrategy::Skip)?;
    let skipped: Vec<&str> = index
        .skipped_text_files()
        .map(|file| file.path().as_str())
        .collect();
    assert_eq!(skipped, ["long.txt", "wide.rs"]);
    let searched: Vec<&str> = index
        .searched_text_files()
        .map(|file| file.path().as_str())
        .collect();
    assert_eq!(searched, ["lib.rs"]);
    assert_eq!(text_rows(&index), ["lib.rs"]);
    let wide = rift_core::ProjectPath::new("wide.rs")?;
    assert!(
        index.file(&wide).is_some(),
        "a declaration of a skipped file still answers"
    );
    assert_eq!(
        index.warnings(),
        [WorkspaceIndexWarning::FileTooLarge(
            rift_core::ProjectPath::new("big.rs")?
        )],
        "a file past max_file leaves the index under skip"
    );
    assert!(
        index.chunked_text_files().is_empty(),
        "nothing is split under skip"
    );
    Ok(())
}
