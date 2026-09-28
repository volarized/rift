use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::Path;

use rift_core::{LanguageFileSelections, SourceVisibility, TextFileInclusion};
use rift_history::fixture::{commit_all, git, init};
use rift_history_store::{DeclarationChange, HeldCommit, RenamedPath};
use rift_protocol::configuration::{HistoryConfiguration, HistoryStrategy};
use rift_protocol::read::SymbolVersionKind;
use rift_syntax::SyntaxLimits;

use super::{HistoryAnalysis, release_version};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn write(root: &Path, path: &str, text: &str) -> TestResult {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)?;
    Ok(())
}

fn analysis(root: &Path, history: &HistoryConfiguration) -> TestResult<HistoryAnalysis> {
    let analysis = HistoryAnalysis::open(
        root,
        history,
        (
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
        ),
        SyntaxLimits::default(),
    )?;
    Ok(analysis)
}

fn everything(max_revisions: u64) -> HistoryConfiguration {
    HistoryConfiguration {
        max_revisions,
        ..HistoryConfiguration::default()
    }
}

fn selective(max_revisions: u64, releases: &[&str]) -> HistoryConfiguration {
    HistoryConfiguration {
        max_revisions,
        strategy: HistoryStrategy::Selective,
        releases: releases
            .iter()
            .map(|release| (*release).to_owned())
            .collect(),
        ..HistoryConfiguration::default()
    }
}

/// Three commits: `beacon` introduced, its body grown, then `gone.rs`
/// deleted, `kept.rs` moved to `moved.rs`, and `notes.txt` edited.
fn three_commits() -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    init(root);
    write(root, "src/lib.rs", "pub fn beacon() {}\n")?;
    write(root, "src/gone.rs", "pub fn gone() {}\n")?;
    write(root, "src/kept.rs", "pub fn kept() {\n    let x = 1;\n}\n")?;
    write(root, "notes.txt", "first\n")?;
    commit_all(root, "introduce beacon");
    write(root, "src/lib.rs", "pub fn beacon() { let _grown = 1; }\n")?;
    commit_all(root, "grow beacon");
    fs::remove_file(root.join("src/gone.rs"))?;
    git(root, &["mv", "src/kept.rs", "src/moved.rs"]);
    write(root, "notes.txt", "second\n")?;
    commit_all(root, "move and delete");
    Ok(directory)
}

fn rev(root: &Path, spelling: &str) -> TestResult<String> {
    let repository = rift_history::Repository::open(root)?;
    Ok(repository.resolve(spelling)?.commit_id())
}

#[test]
fn a_plan_selects_the_window_newest_first_with_each_first_parent() -> TestResult {
    let directory = three_commits()?;
    let root = directory.path();
    let analysis = analysis(root, &everything(2))?;

    let plan = analysis.plan(&HashMap::new())?;

    let pending: Vec<String> = plan
        .pending()
        .iter()
        .map(|pending| pending.revision().commit_id())
        .collect();
    assert_eq!(pending, [rev(root, "HEAD")?, rev(root, "HEAD~1")?]);
    assert_eq!(
        plan.pending()[1].held(),
        HeldCommit {
            base: Some(rev(root, "HEAD~2")?),
            boundary: false
        },
        "the oldest windowed commit is still compared with its parent"
    );
    assert_eq!(plan.keep().len(), 2);
    Ok(())
}

#[test]
fn a_plan_owes_nothing_for_a_commit_held_under_the_same_base() -> TestResult {
    let directory = three_commits()?;
    let root = directory.path();
    let analysis = analysis(root, &everything(10))?;
    let first = analysis.plan(&HashMap::new())?;
    let mut stored: HashMap<String, HeldCommit> = first
        .pending()
        .iter()
        .map(|pending| (pending.revision().commit_id(), pending.held()))
        .collect();

    assert!(analysis.plan(&stored)?.pending().is_empty());

    let head = rev(root, "HEAD")?;
    stored.insert(
        head.clone(),
        HeldCommit {
            base: None,
            boundary: true,
        },
    );
    let replanned = analysis.plan(&stored)?;
    let pending: Vec<String> = replanned
        .pending()
        .iter()
        .map(|pending| pending.revision().commit_id())
        .collect();
    assert_eq!(
        pending,
        [head],
        "a commit held under another base is owed again"
    );
    Ok(())
}

#[test]
fn a_plan_keeps_the_union_of_every_live_worktree_window() -> TestResult {
    let directory = three_commits()?;
    let root = directory.path();
    let others = tempfile::tempdir()?;
    let linked = others.path().join("linked");
    git(
        root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            &linked.display().to_string(),
            "HEAD~1",
        ],
    );
    write(&linked, "src/side.rs", "pub fn side() {}\n")?;
    commit_all(&linked, "work on the side");
    let side = rev(&linked, "HEAD")?;

    let plan = analysis(root, &everything(1))?.plan(&HashMap::new())?;

    let pending: Vec<String> = plan
        .pending()
        .iter()
        .map(|pending| pending.revision().commit_id())
        .collect();
    assert_eq!(pending, [rev(root, "HEAD")?, side.clone()]);
    assert!(plan.keep().contains(&side));
    Ok(())
}

#[test]
fn a_selective_plan_keeps_the_newest_releases_in_version_order() -> TestResult {
    let directory = three_commits()?;
    let root = directory.path();
    git(root, &["tag", "v0.0.2", "HEAD~2"]);
    git(root, &["tag", "v0.0.10", "HEAD"]);
    git(root, &["tag", "v0.0.9", "HEAD~1"]);
    git(root, &["tag", "v0.0.x", "HEAD~1"]);
    git(root, &["tag", "other-1.0.0", "HEAD~1"]);

    let plan = analysis(root, &selective(2, &["v0.0.*"]))?.plan(&HashMap::new())?;

    let pending: Vec<(String, HeldCommit)> = plan
        .pending()
        .iter()
        .map(|pending| (pending.revision().commit_id(), pending.held()))
        .collect();
    assert_eq!(
        pending,
        [
            (
                rev(root, "HEAD")?,
                HeldCommit {
                    base: Some(rev(root, "HEAD~1")?),
                    boundary: false
                }
            ),
            (
                rev(root, "HEAD~1")?,
                HeldCommit {
                    base: None,
                    boundary: true
                }
            ),
        ],
        "v0.0.10 follows v0.0.9 in version order, and the oldest kept release is compared with nothing"
    );
    assert_eq!(plan.releases_past_bound(), 1);
    let unversioned: Vec<(&str, &str)> = plan
        .unversioned()
        .iter()
        .map(|tag| (tag.name.as_str(), tag.remainder.as_str()))
        .collect();
    assert_eq!(unversioned, [("v0.0.x", "0.0.x")]);
    Ok(())
}

#[test]
fn a_release_pattern_that_is_no_glob_refuses_the_analysis() -> TestResult {
    let directory = three_commits()?;

    let refused = analysis(directory.path(), &selective(2, &["v[0"]))
        .err()
        .ok_or("an unclosed class is no tag pattern")?;

    assert!(
        refused.to_string().contains("providers.history.releases"),
        "{refused}"
    );
    Ok(())
}

#[test]
fn a_release_version_strips_the_patterns_literal_text_up_to_its_first_digit() {
    let parsed = |tag: &str, pattern: &str| release_version(tag, pattern).map(|v| v.to_string());
    assert_eq!(parsed("1.80.0", "1.*"), Ok("1.80.0".to_owned()));
    assert_eq!(parsed("1.80.0", "1.*.*"), Ok("1.80.0".to_owned()));
    assert_eq!(parsed("tokio-1.38.0", "tokio-1.*"), Ok("1.38.0".to_owned()));
    assert_eq!(parsed("v0.0.45", "v0.0.*"), Ok("0.0.45".to_owned()));
    assert_eq!(parsed("v0.0.45", "v*"), Ok("0.0.45".to_owned()));
    assert_eq!(parsed("v0.0.45", "v*.*.*"), Ok("0.0.45".to_owned()));
    assert_eq!(
        parsed("tokio-0.2.0-alpha.6", "tokio-*-alpha*"),
        Ok("0.2.0-alpha.6".to_owned())
    );
    assert_eq!(parsed("v0.0.45", "v0.0.45"), Ok("0.0.45".to_owned()));
    assert_eq!(parsed("0.10", "0.*"), Err("0.10".to_owned()));
    assert_eq!(
        parsed("tokio-util-0.7.0", "tokio-*"),
        Err("util-0.7.0".to_owned())
    );
}

#[test]
fn an_analyzed_commit_classifies_each_changed_declaration_and_pairs_pure_renames() -> TestResult {
    let directory = three_commits()?;
    let root = directory.path();
    let analysis = analysis(root, &everything(10))?;
    let plan = analysis.plan(&HashMap::new())?;

    let head = analysis
        .analyze(&plan.pending()[0], &|| false)?
        .ok_or("nothing cancels the analysis")?;
    let grown = analysis
        .analyze(&plan.pending()[1], &|| false)?
        .ok_or("nothing cancels the analysis")?;
    let root_commit = analysis
        .analyze(&plan.pending()[2], &|| false)?
        .ok_or("nothing cancels the analysis")?;

    let record = head.record();
    assert_eq!(record.author.name, "Rift Fixture");
    assert_eq!(record.message, "move and delete\n");
    let paths: Vec<&str> = record.paths.iter().map(|path| path.path.as_str()).collect();
    assert_eq!(
        paths,
        ["notes.txt", "src/gone.rs", "src/kept.rs", "src/moved.rs"]
    );
    assert_eq!(
        record.renames,
        [RenamedPath {
            old_path: "src/kept.rs".to_owned(),
            new_path: "src/moved.rs".to_owned()
        }]
    );
    assert_eq!(
        record.declarations,
        [DeclarationChange {
            path: "src/gone.rs".to_owned(),
            qualified_name: "gone".to_owned(),
            change: SymbolVersionKind::Removed
        }],
        "a pure rename parses nothing and a text file has no provider"
    );
    assert_eq!(
        grown.record().declarations,
        [DeclarationChange {
            path: "src/lib.rs".to_owned(),
            qualified_name: "beacon".to_owned(),
            change: SymbolVersionKind::BodyChanged
        }]
    );
    assert!(grown.parsed_bytes() > 0);
    let introduced: Vec<&str> = root_commit
        .record()
        .declarations
        .iter()
        .map(|change| change.qualified_name.as_str())
        .collect();
    assert_eq!(introduced, ["gone", "kept", "beacon"], "in path order");
    assert!(
        root_commit
            .record()
            .declarations
            .iter()
            .all(|change| change.change == SymbolVersionKind::Introduced)
    );
    Ok(())
}

#[test]
fn an_analysis_asked_to_stop_answers_nothing() -> TestResult {
    let directory = three_commits()?;
    let analysis = analysis(directory.path(), &everything(10))?;
    let plan = analysis.plan(&HashMap::new())?;

    assert!(analysis.analyze(&plan.pending()[0], &|| true)?.is_none());
    Ok(())
}

#[test]
fn a_release_compared_with_nothing_writes_its_facts_alone() -> TestResult {
    let directory = three_commits()?;
    let root = directory.path();
    git(root, &["tag", "v1.0.0", "HEAD"]);
    let analysis = analysis(root, &selective(10, &["v*"]))?;
    let plan = analysis.plan(&HashMap::new())?;

    let analyzed = analysis
        .analyze(&plan.pending()[0], &|| false)?
        .ok_or("nothing cancels the analysis")?;

    assert!(analyzed.record().boundary);
    assert!(analyzed.record().paths.is_empty());
    assert_eq!(analyzed.parsed_bytes(), 0);
    Ok(())
}

#[test]
fn a_blob_past_the_syntax_bound_answers_no_declaration() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    init(root);
    write(root, "src/lib.rs", "pub fn small() {}\n")?;
    commit_all(root, "small");
    let grown = format!("pub fn small() {{\n    // {}\n}}\n", "x".repeat(4_096));
    write(root, "src/lib.rs", &grown)?;
    commit_all(root, "grown");
    let analysis = HistoryAnalysis::open(
        root,
        &everything(10),
        (
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
        ),
        SyntaxLimits::new(1_024, 250_000, 512)?,
    )?;
    let plan = analysis.plan(&HashMap::new())?;

    let analyzed = analysis
        .analyze(&plan.pending()[0], &|| false)?
        .ok_or("nothing cancels the analysis")?;

    assert_eq!(analyzed.record().paths.len(), 1);
    assert!(analyzed.record().declarations.is_empty(), "{analyzed:?}");
    Ok(())
}

/// One commit on top of `from.rs` holding `travelled` and `stays`: `from.rs`
/// deleted, and `added` written, each `(path, text)`.
fn moved_between_files(added: &[(&str, &str)]) -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    init(root);
    write(
        root,
        "src/from.rs",
        "pub fn travelled() {\n    let x = 1;\n}\npub fn stays() {}\n",
    )?;
    commit_all(root, "introduce travelled");
    fs::remove_file(root.join("src/from.rs"))?;
    for (path, text) in added {
        write(root, path, text)?;
    }
    commit_all(root, "move travelled");
    Ok(directory)
}

fn head_record(analysis: &HistoryAnalysis) -> TestResult<rift_history_store::CommitRecord> {
    let plan = analysis.plan(&HashMap::new())?;
    let analyzed = analysis
        .analyze(&plan.pending()[0], &|| false)?
        .ok_or("nothing cancels the analysis")?;
    Ok(analyzed.into_record())
}

#[test]
fn a_declaration_moved_unchanged_into_a_new_file_pairs_one_to_one() -> TestResult {
    let directory = moved_between_files(&[(
        "src/to.rs",
        "pub fn travelled() {\n    let x = 1;\n}\npub fn arrived() {}\n",
    )])?;

    let record = head_record(&analysis(directory.path(), &everything(10))?)?;

    assert!(record.renames.is_empty(), "the files' bytes differ");
    assert_eq!(
        record.moves,
        [rift_history_store::MovedDeclaration {
            old_path: "src/from.rs".to_owned(),
            new_path: "src/to.rs".to_owned(),
            qualified_name: "travelled".to_owned(),
        }]
    );
    let changes: Vec<(&str, &str, SymbolVersionKind)> = record
        .declarations
        .iter()
        .map(|change| {
            (
                change.path.as_str(),
                change.qualified_name.as_str(),
                change.change,
            )
        })
        .collect();
    assert_eq!(
        changes,
        [
            ("src/from.rs", "stays", SymbolVersionKind::Removed),
            ("src/from.rs", "travelled", SymbolVersionKind::Removed),
            ("src/to.rs", "arrived", SymbolVersionKind::Introduced),
            ("src/to.rs", "travelled", SymbolVersionKind::Moved),
        ]
    );
    Ok(())
}

#[test]
fn an_ambiguous_or_edited_move_pairs_nothing() -> TestResult {
    let copied = "pub fn travelled() {\n    let x = 1;\n}\n";
    let ambiguous = moved_between_files(&[("src/one.rs", copied), ("src/two.rs", copied)])?;
    let edited =
        moved_between_files(&[("src/to.rs", "pub fn travelled() {\n        let x = 1;\n}\n")])?;

    let ambiguous = head_record(&analysis(ambiguous.path(), &everything(10))?)?;
    let edited = head_record(&analysis(edited.path(), &everything(10))?)?;

    assert!(
        ambiguous.moves.is_empty(),
        "two additions answer one removal"
    );
    assert!(
        edited.moves.is_empty(),
        "an indentation change is a changed declaration"
    );
    assert!(
        edited
            .declarations
            .iter()
            .any(|change| change.path == "src/to.rs"
                && change.change == SymbolVersionKind::Introduced)
    );
    Ok(())
}

#[test]
fn a_commit_past_the_deleted_file_bound_pairs_no_move() -> TestResult {
    let directory = moved_between_files(&[(
        "src/to.rs",
        "pub fn travelled() {\n    let x = 1;\n}\npub fn arrived() {}\n",
    )])?;

    let bounded = analysis(directory.path(), &everything(10))?.with_move_deletions_max(0);
    let record = head_record(&bounded)?;

    assert!(record.moves.is_empty());
    assert!(record.declarations.iter().any(|change| {
        change.qualified_name == "travelled" && change.change == SymbolVersionKind::Introduced
    }));
    Ok(())
}

#[test]
fn the_deleted_file_bound_counts_the_deletions_pure_renames_leave() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    init(root);
    write(
        root,
        "src/from.rs",
        "pub fn travelled() {\n    let x = 1;\n}\npub fn stays() {}\n",
    )?;
    write(root, "src/renamed.rs", "pub fn renamed() {}\n")?;
    commit_all(root, "introduce");
    fs::remove_file(root.join("src/from.rs"))?;
    git(root, &["mv", "src/renamed.rs", "src/moved.rs"]);
    write(
        root,
        "src/to.rs",
        "pub fn travelled() {\n    let x = 1;\n}\npub fn arrived() {}\n",
    )?;
    commit_all(root, "move both");

    let bounded = analysis(root, &everything(10))?.with_move_deletions_max(1);
    let record = head_record(&bounded)?;

    assert_eq!(record.renames.len(), 1, "the pure rename pairs by blob id");
    assert_eq!(
        record.moves.len(),
        1,
        "two files were deleted, and the one the pure rename leaves is within a bound of one"
    );
    Ok(())
}
