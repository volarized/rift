//! The trigram index a regex pattern's prefilter reads: its tokens equal the trigram rule
//! the prefilter is built from, its rows stay in step with every write, and its candidate
//! selection loses no file a match could sit in.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use regex_syntax::hir::{ClassUnicode, ClassUnicodeRange};
use rift_core::{LanguageFileSelections, ProjectPath, SourceVisibility, TextFileInclusion};
use rift_index::{
    DatabasePool, LexicalChange, LexicalIndexLimits, LexicalSearchIndex, LexicalStamp,
    PatternCandidates, RevisionScoped, WorkspaceDatabase, WorkspaceIndex, WorkspaceIndexLimits,
};
use rift_ranking::{
    DocumentFields, DocumentIdentity, DocumentKind, DocumentLocation, IndexDocument, Pattern,
    Prefilter, ROW_EXPRESSION_DEPTH_MAX, SearchableField, TRIGRAM_TOKENIZER, fold, trigram_set,
};
use tempfile::TempDir;
use toasty::Db;
use toasty::stmt::{Type, Value};
use toasty_driver_sqlite::Sqlite;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// The smallest chunk bound the configuration accepts, so a few kilobytes split into rows.
const CHUNK_BYTES_MAX: u64 = 1_024;

/// A compiled-size bound far above every pattern this suite runs.
const SIZE_LIMIT: usize = 1 << 20;

/// A row bound far above every selection this suite makes.
const ROWS_MAX: u32 = 10_000;

/// The case classes `regex-syntax` 0.8.11 builds for `(?i)`: every character's simple case
/// folding orbit holding more than one member, as the text-search evaluation counted them.
const CASE_CLASSES: usize = 1_454;

/// The members of [`CASE_CLASSES`], each checked against the bundled tokenizer.
const CASE_CLASS_MEMBERS: usize = 2_938;

async fn probe(path: &Path) -> TestResult<Db> {
    Ok(Db::builder().build(Sqlite::open(path)).await?)
}

/// Every simple case folding orbit of more than one character, in code point order.
fn case_classes() -> Vec<Vec<char>> {
    let mut classes = BTreeSet::new();
    for character in (0..=0x10_FFFF).filter_map(char::from_u32) {
        let mut class = ClassUnicode::new([ClassUnicodeRange::new(character, character)]);
        class.case_fold_simple();
        let members: Vec<char> = class
            .ranges()
            .iter()
            .flat_map(|range| range.start()..=range.end())
            .collect();
        if members.len() > 1 {
            classes.insert(members);
        }
    }
    classes.into_iter().collect()
}

/// Each row's tokens, as the bundled `trigram` tokenizer wrote them into a scratch table
/// holding `texts`, keyed by row id.
async fn tokenizer_terms(texts: &[String]) -> TestResult<BTreeMap<i64, BTreeSet<String>>> {
    let directory = TempDir::new()?;
    let database = probe(&directory.path().join("tokens.db")).await?;
    let mut connection = database.connection().await?;
    toasty::sql::statement(format!(
        "CREATE VIRTUAL TABLE scratch USING fts5(text, tokenize='{TRIGRAM_TOKENIZER}')"
    ))
    .exec(&mut connection)
    .await?;
    toasty::sql::statement(
        "CREATE VIRTUAL TABLE scratch_terms USING fts5vocab(scratch, 'instance')",
    )
    .exec(&mut connection)
    .await?;
    for (row, text) in texts.iter().enumerate() {
        toasty::sql::statement("INSERT INTO scratch(rowid, text) VALUES (?1, ?2)")
            .bind(i64::try_from(row)?)
            .bind(text.clone())
            .exec(&mut connection)
            .await?;
    }
    let rows = toasty::sql::query("SELECT doc, term FROM scratch_terms")
        .column_types([Type::I64, Type::String])
        .exec(&mut connection)
        .await?;
    let mut terms: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    for row in rows {
        let Value::Record(record) = row else {
            return Err("a vocabulary row reads back as a record".into());
        };
        let [Value::I64(doc), Value::String(term)] = record.as_slice() else {
            return Err(format!("unexpected vocabulary row: {record:?}").into());
        };
        terms.entry(*doc).or_default().insert(term.clone());
    }
    Ok(terms)
}

/// The trigram rule folds every member of every case class to the token the bundled
/// tokenizer writes for it: three copies of one member are one trigram, and the table's
/// token for that row equals the rule's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fold_equals_the_bundled_tokenizer_for_every_case_class_member() -> TestResult {
    let members: Vec<char> = case_classes().into_iter().flatten().collect();
    assert_eq!(case_classes().len(), CASE_CLASSES);
    assert_eq!(members.len(), CASE_CLASS_MEMBERS);
    let texts: Vec<String> = members
        .iter()
        .map(|member| member.to_string().repeat(3))
        .collect();
    let terms = tokenizer_terms(&texts).await?;
    let mut disagreements = Vec::new();
    for (row, member) in members.iter().enumerate() {
        let expected = fold(*member).to_string().repeat(3);
        let written = terms.get(&i64::try_from(row)?);
        if written != Some(&BTreeSet::from([expected.clone()])) {
            disagreements.push((*member, expected, written.cloned()));
        }
    }
    assert!(
        disagreements.is_empty(),
        "{} of {} members fold apart from the tokenizer: {:?}",
        disagreements.len(),
        members.len(),
        &disagreements[..disagreements.len().min(8)]
    );
    Ok(())
}

/// Over a fixed corpus of mixed case, multibyte, CRLF, and folded text, every row's
/// tokens are exactly the trigrams the rule derives from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_table_tokens_of_a_fixed_corpus_are_the_rule_trigrams() -> TestResult {
    let texts: Vec<String> = [
        "pub fn Beacon() -> Result<(), Error> {}",
        "let a = 1;\r\nlet b = 2;\r\n",
        "gr\u{fc}\u{df}e \u{65e5}\u{672c}\u{8a9e} TODO",
        "\u{212a}elvin \u{17f}ign \u{10d0}\u{1c90}",
        "  spaces\tand\ttabs  ",
    ]
    .map(str::to_owned)
    .to_vec();
    let terms = tokenizer_terms(&texts).await?;
    for (row, text) in texts.iter().enumerate() {
        assert_eq!(
            terms.get(&i64::try_from(row)?),
            Some(&trigram_set(text)),
            "{text:?}"
        );
    }
    Ok(())
}

fn pool() -> DatabasePool {
    DatabasePool::new(2, 5_000)
}

fn database_path(directory: &TempDir) -> PathBuf {
    directory.path().join("lexical.db")
}

async fn store(directory: &TempDir) -> TestResult<LexicalSearchIndex> {
    Ok(LexicalSearchIndex::attached(
        WorkspaceDatabase::open(&database_path(directory), pool()).await?,
        LexicalIndexLimits::default(),
    ))
}

/// FTS5's own check of the trigram index against the rows it reads: every text row's
/// trigrams, and nothing a removed row held.
async fn assert_trigram_index_matches_rows(path: &Path) -> TestResult {
    let database = probe(path).await?;
    let mut connection = database.connection().await?;
    toasty::sql::statement(
        "INSERT INTO lexical_documents_trigram(lexical_documents_trigram, rank) \
         VALUES('integrity-check', 1)",
    )
    .exec(&mut connection)
    .await?;
    Ok(())
}

/// Writes `files` below a fresh tree and indexes it with every file as text, chunked at
/// [`CHUNK_BYTES_MAX`].
fn chunked_tree(files: &[(&str, String)]) -> TestResult<(TempDir, WorkspaceIndex)> {
    let tree = TempDir::new()?;
    for (name, text) in files {
        std::fs::write(tree.path().join(name), text)?;
    }
    let index = WorkspaceIndex::build_with_languages(
        tree.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::new(vec!["**".to_owned()], CHUNK_BYTES_MAX),
        &LanguageFileSelections::default(),
    )?;
    Ok((tree, index))
}

/// Forty filler lines, then `needle`, then forty more: past the chunk bound, with
/// `needle` alone in a later chunk.
fn long_text(needle: &str) -> String {
    let filler = |lines: std::ops::Range<u32>| {
        lines.fold(String::new(), |mut text, line| {
            let _ = writeln!(text, "filler line {line:03} of plain text");
            text
        })
    };
    format!("{}{needle}\n{}", filler(0..40), filler(40..80))
}

async fn candidates(
    index: &LexicalSearchIndex,
    tree_revision: &str,
    pattern: &str,
    rows_max: u32,
) -> TestResult<PatternCandidates> {
    let pattern = Pattern::parse(pattern, SIZE_LIMIT)?;
    let prefilter = pattern.prefilter().ok_or("the pattern has a prefilter")?;
    match index
        .pattern_candidates(tree_revision, prefilter, pattern.is_line_bound(), rows_max)
        .await?
    {
        RevisionScoped::Matched(candidates) => Ok(candidates),
        other => Err(format!("the store must hold {tree_revision}: {other:?}").into()),
    }
}

fn selected(candidates: &PatternCandidates) -> Vec<(&str, Vec<std::ops::Range<u64>>)> {
    candidates
        .candidates()
        .iter()
        .map(|candidate| (candidate.path().as_str(), candidate.spans().to_vec()))
        .collect()
}

/// A line-bound pattern selects the one row of a chunked file that holds it, with the
/// bytes of the file that row holds, and a case-insensitive pattern selects the same row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_line_bound_pattern_selects_the_rows_holding_it_with_their_spans() -> TestResult {
    let long = long_text("the Beacon lantern sits here");
    let (_tree, workspace) = chunked_tree(&[
        ("long.txt", long.clone()),
        ("short.txt", "no light at all\n".to_owned()),
    ])?;
    let directory = TempDir::new()?;
    let index = store(&directory).await?;
    index
        .replace_all(&workspace.index_documents(), "revision-1")
        .await?;
    assert_trigram_index_matches_rows(&database_path(&directory)).await?;

    let at = u64::try_from(long.find("Beacon").ok_or("the text holds the needle")?)?;
    for pattern in [r"Beacon\s+lantern", "(?i)beacon LANTERN"] {
        let found = candidates(&index, "revision-1", pattern, ROWS_MAX).await?;
        let rows = selected(&found);
        let [("long.txt", spans)] = rows.as_slice() else {
            panic!("one file holds {pattern:?}: {rows:?}");
        };
        let [span] = spans.as_slice() else {
            panic!("one row holds {pattern:?}: {spans:?}");
        };
        assert!(span.start > 0 && span.contains(&at), "{span:?} holds {at}");
        assert_eq!(found.truncated_at(), None);
    }
    let absent = candidates(&index, "revision-1", "lighthouse", ROWS_MAX).await?;
    assert!(absent.candidates().is_empty());
    Ok(())
}

/// A pattern holding a line feed may cross two rows, so each literal runs its own match
/// and the file holding every literal answers whole, here across the boundary between its
/// first two chunks; a file holding one literal alone is not selected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pattern_crossing_a_line_selects_whole_files_across_chunks() -> TestResult {
    let (_tree, workspace) = chunked_tree(&[
        ("long.txt", long_text("harbor relay ends")),
        ("half.txt", "filler line 033 of plain text\n".to_owned()),
    ])?;
    let directory = TempDir::new()?;
    let index = store(&directory).await?;
    index
        .replace_all(&workspace.index_documents(), "revision-1")
        .await?;
    // Thirty-byte filler lines pack 34 to the first 1024-byte chunk, so line 033 ends it
    // and line 034 opens the next.
    let row_of = |found: &PatternCandidates| {
        found
            .candidates()
            .iter()
            .find(|candidate| candidate.path().as_str() == "long.txt")
            .map(|candidate| candidate.spans().to_vec())
    };
    let first = candidates(&index, "revision-1", "filler line 033", ROWS_MAX).await?;
    let second = candidates(&index, "revision-1", "filler line 034", ROWS_MAX).await?;
    assert_ne!(
        row_of(&first),
        row_of(&second),
        "the two lines sit in two rows"
    );

    let crossing = "033 of plain text\\nfiller line 034";
    let found = candidates(&index, "revision-1", crossing, ROWS_MAX).await?;
    assert_eq!(selected(&found), [("long.txt", Vec::new())]);
    Ok(())
}

/// The candidate bound counts the rows the match reads, not the files they belong to:
/// a pattern held by every chunk of one file passes a bound one row short of them, and
/// the selection then carries no candidate at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_candidate_bound_counts_rows_and_refuses_a_cut_selection() -> TestResult {
    let (_tree, workspace) = chunked_tree(&[("long.txt", long_text("marker line"))])?;
    let directory = TempDir::new()?;
    let index = store(&directory).await?;
    index
        .replace_all(&workspace.index_documents(), "revision-1")
        .await?;
    let every_row = candidates(&index, "revision-1", "filler line", ROWS_MAX).await?;
    let rows = u32::try_from(every_row.candidates()[0].spans().len())?;
    assert!(rows > 1, "every chunk holds the pattern: {rows}");
    assert_eq!(every_row.candidates().len(), 1, "one file");

    let exact = candidates(&index, "revision-1", "filler line", rows).await?;
    assert_eq!(
        exact.truncated_at(),
        None,
        "a bound equal to the rows holds"
    );
    let cut = candidates(&index, "revision-1", "filler line", rows - 1).await?;
    assert_eq!(cut.truncated_at(), Some(rows - 1));
    assert!(
        cut.candidates().is_empty(),
        "a cut selection answers nothing"
    );

    let crossing = candidates(&index, "revision-1", "filler line\\nof plain text", rows).await?;
    assert_eq!(
        crossing.truncated_at(),
        Some(rows),
        "every literal's match counts against one bound"
    );
    Ok(())
}

/// A formula nested past the depth one match renders runs one match per literal, and a
/// file meeting the formula across its rows answers whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_formula_past_the_depth_bound_selects_by_literal() -> TestResult {
    let (_tree, workspace) =
        chunked_tree(&[("notes.txt", "alpha beacon\nomega relay\n".to_owned())])?;
    let directory = TempDir::new()?;
    let index = store(&directory).await?;
    index
        .replace_all(&workspace.index_documents(), "revision-1")
        .await?;
    let mut formula = Prefilter::Literal(trigram_set("beacon"));
    for level in 0..=ROW_EXPRESSION_DEPTH_MAX {
        let sibling = Prefilter::Literal(trigram_set(&format!("absent {level}")));
        formula = if level % 2 == 0 {
            Prefilter::Any(vec![formula, sibling])
        } else {
            Prefilter::All(vec![formula, Prefilter::Literal(trigram_set("relay"))])
        };
    }
    assert_eq!(formula.row_expression(), None);
    let RevisionScoped::Matched(found) = index
        .pattern_candidates("revision-1", &formula, true, ROWS_MAX)
        .await?
    else {
        return Err("the store holds revision-1".into());
    };
    assert_eq!(selected(&found), [("notes.txt", Vec::new())]);
    Ok(())
}

/// A row that stores text without recording where it starts in its file, as a notebook
/// cell's row does, sits in the trigram index yet selects no file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_recording_no_offset_selects_no_file() -> TestResult {
    let directory = TempDir::new()?;
    let index = store(&directory).await?;
    let fields = DocumentFields::empty()
        .with(SearchableField::Name, "notes.ipynb")
        .with(SearchableField::FileContent, "print('beacon')");
    let cell = IndexDocument::new(
        DocumentIdentity::new("notes.ipynb#cell-1")?,
        DocumentLocation::Project(ProjectPath::new("notes.ipynb")?),
        DocumentKind::TextFile,
        fields.digest(),
        fields,
    )?;
    index.replace_all(&[cell], "revision-1").await?;
    assert_trigram_index_matches_rows(&database_path(&directory)).await?;
    let found = candidates(&index, "revision-1", "beacon", ROWS_MAX).await?;
    assert!(found.candidates().is_empty(), "{found:?}");
    Ok(())
}

/// The trigram index stays in step with the rows through a replace, an apply that
/// rewrites and removes paths, a repeated apply, and the clear and refill a new derivation
/// revision runs, and a selection answers for the tree it names alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_trigram_index_matches_its_rows_after_every_replace_and_apply() -> TestResult {
    let (tree, workspace) = chunked_tree(&[
        ("long.txt", long_text("first beacon")),
        ("gone.txt", "gone beacon\n".to_owned()),
    ])?;
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let index = store(&directory).await?;
    index
        .replace_all(&workspace.index_documents(), "revision-1")
        .await?;
    assert_trigram_index_matches_rows(&path).await?;

    std::fs::write(tree.path().join("long.txt"), long_text("second lantern"))?;
    std::fs::remove_file(tree.path().join("gone.txt"))?;
    let rebuilt = WorkspaceIndex::build_with_languages(
        tree.path(),
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::new(vec!["**".to_owned()], CHUNK_BYTES_MAX),
        &LanguageFileSelections::default(),
    )?;
    let replaced = vec![ProjectPath::new("long.txt")?, ProjectPath::new("gone.txt")?];
    let change = LexicalChange::new(replaced.clone(), rebuilt.index_documents_for(&replaced));
    let stamp = LexicalStamp::published("revision-2", "derivation-a");
    index.apply(&change, &stamp).await?;
    assert_trigram_index_matches_rows(&path).await?;
    index
        .apply(
            &change,
            &LexicalStamp::published("revision-3", "derivation-a"),
        )
        .await?;
    assert_trigram_index_matches_rows(&path).await?;

    let lantern = candidates(&index, "revision-3", "second lantern", ROWS_MAX).await?;
    assert_eq!(lantern.candidates().len(), 1);
    let beacon = candidates(&index, "revision-3", "beacon", ROWS_MAX).await?;
    assert!(
        beacon.candidates().is_empty(),
        "no row holds a removed text"
    );
    let pattern = Pattern::parse("lantern", SIZE_LIMIT)?;
    let prefilter = pattern.prefilter().ok_or("a prefilter")?;
    assert_eq!(
        index
            .pattern_candidates("revision-2", prefilter, true, ROWS_MAX)
            .await?,
        RevisionScoped::OtherRevision("revision-3".to_owned())
    );

    // A binary with another executable digest derives under another derivation revision:
    // it finds nothing it can keep, clears the store, and fills it again. The trigram
    // index carries no revision of its own and is cleared and refilled with the rows.
    assert_eq!(index.recorded_files("derivation-b").await?, None);
    index.clear("derivation-b").await?;
    assert_trigram_index_matches_rows(&path).await?;
    assert_eq!(
        index
            .pattern_candidates("revision-3", prefilter, true, ROWS_MAX)
            .await?,
        RevisionScoped::NoRevision
    );
    let reloaded = LexicalChange::new(replaced.clone(), rebuilt.index_documents_for(&replaced));
    index
        .apply(
            &reloaded,
            &LexicalStamp::published("revision-4", "derivation-b"),
        )
        .await?;
    assert_trigram_index_matches_rows(&path).await?;
    let found = candidates(&index, "revision-4", "second lantern", ROWS_MAX).await?;
    assert_eq!(
        found.candidates().len(),
        1,
        "the reload refills the trigram index"
    );
    Ok(())
}

/// A pattern search verifies whole the files the trigram index cannot rule out: a
/// notebook, whose rows hold its cells, and a file holding a line past the chunk bound,
/// which chunking cut mid-line. A line exactly at the bound, its ending included, stays
/// whole and needs no such reading.
#[test]
fn whole_file_candidates_name_notebooks_and_files_holding_a_cut_line() -> TestResult {
    let chunk = usize::try_from(CHUNK_BYTES_MAX)?;
    let at_bound = format!("{}\n", "a".repeat(chunk - 1));
    let past_bound = format!("{}\n", "b".repeat(chunk));
    let (_tree, workspace) = chunked_tree(&[
        ("fits.txt", at_bound.repeat(3)),
        ("cut.txt", format!("head\n{past_bound}tail\n")),
        (
            "notes.ipynb",
            r#"{"cells":[],"metadata":{},"nbformat":4,"nbformat_minor":5}"#.to_owned(),
        ),
        ("small.txt", "x\n".to_owned()),
    ])?;
    let whole: Vec<&str> = workspace
        .whole_file_candidates()
        .map(|file| file.path().as_str())
        .collect();
    assert_eq!(whole, ["cut.txt", "notes.ipynb"]);
    let searched: Vec<&str> = workspace
        .searched_text_files()
        .map(|file| file.path().as_str())
        .collect();
    assert_eq!(
        searched,
        ["cut.txt", "fits.txt", "notes.ipynb", "small.txt"]
    );
    Ok(())
}
