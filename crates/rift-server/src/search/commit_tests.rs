use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rift_core::{LanguageFileSelections, SourceVisibility, TextFileInclusion};
use rift_error::errors;
use rift_history::fixture::{commit_all, commit_all_at, git, init};
use rift_history_store::{HistoryStore, StoreLocation};
use rift_index::WorkspaceIndexLimits;
use rift_protocol::configuration::{HistoryConfiguration, HistoryStrategy};
use rift_protocol::read::{
    COMMIT_MESSAGE_BYTES_MAX, COMMIT_PATHS_MAX, CommitHit, ReadWarning, SearchHitTarget,
    SearchParams, SearchResult,
};
use rift_syntax::SyntaxLimits;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::commit::{commit_conflict, commit_hit};
use crate::HistoryAnalysis;
use crate::history::{FillProgress, StoredHistory};
use crate::read::ReadService;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn write(root: &Path, path: &str, text: &str) -> TestResult {
    let path = root.join(path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text)?;
    Ok(())
}

/// Three commits a minute apart: `beacon` introduced, its body grown beside a lockfile
/// edit, then the release notes fixed.
fn three_commits() -> TestResult<TempDir> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    init(root);
    write(root, "src/lib.rs", "pub fn beacon() {}\n")?;
    write(root, "package-lock.json", "{\"lockfileVersion\": 3}\n")?;
    commit_all_at(
        root,
        "Introduce the beacon\n\nThe beacon answers every ping.\n",
        "2026-01-01T00:01:00 +0000",
    );
    write(root, "src/lib.rs", "pub fn beacon() { let _grown = 1; }\n")?;
    write(root, "src/ping.rs", "pub fn ping() {}\n")?;
    write(
        root,
        "package-lock.json",
        "{\"lockfileVersion\": 3, \"packages\": {}}\n",
    )?;
    commit_all_at(root, "Grow the beacon body\n", "2026-01-01T00:02:00 +0000");
    write(root, "NOTES.md", "# Notes\n")?;
    commit_all_at(root, "Fix the release notes\n", "2026-01-01T00:03:00 +0000");
    Ok(directory)
}

/// A history store under `folder`, filled with every commit `history` selects in the
/// workspace at `root`.
fn filled_store(
    root: &Path,
    folder: &Path,
    history: &HistoryConfiguration,
) -> TestResult<HistoryStore> {
    let store = HistoryStore::open(&StoreLocation::new(folder, "aa"))?;
    let mut filler = store.filler()?.ok_or("no other filler runs")?;
    let analysis = HistoryAnalysis::open(
        root,
        history,
        (
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
        ),
        SyntaxLimits::default(),
    )
    .map_err(|error| error.to_string())?;
    let plan = analysis
        .plan(&filler.held()?)
        .map_err(|error| error.to_string())?;
    let mut records = Vec::new();
    for pending in plan.pending() {
        let analyzed = analysis
            .analyze(pending, &|| false)
            .map_err(|error| error.to_string())?
            .ok_or("nothing cancels the analysis")?;
        records.push(analyzed.into_record());
    }
    filler.write_batch(&records)?;
    Ok(store)
}

/// A current-tree snapshot of `root` under `limits` and `history`, with `store` attached
/// when there is one.
fn service(
    root: &Path,
    limits: WorkspaceIndexLimits,
    history: HistoryConfiguration,
    store: Option<&HistoryStore>,
) -> TestResult<ReadService> {
    let strategy = history.strategy;
    let service = ReadService::build(
        root,
        limits,
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        history,
    )?;
    if let Some(store) = store {
        service.attach_history_store(&StoredHistory::new(
            store.reader(),
            strategy,
            Arc::new(|| {}),
        ));
    }
    Ok(service)
}

/// A snapshot of `root` whose store holds every commit under the default table.
fn searchable(root: &Path, folder: &Path) -> TestResult<ReadService> {
    let history = HistoryConfiguration::default();
    let store = filled_store(root, folder, &history)?;
    service(root, WorkspaceIndexLimits::default(), history, Some(&store))
}

fn commit_search(query: &str) -> TestResult<SearchParams> {
    Ok(serde_json::from_value(
        json!({"target": "commit", "query": query}),
    )?)
}

/// Each hit's commit, in answer order.
fn commits(answer: &SearchResult) -> Vec<&CommitHit> {
    answer
        .results
        .iter()
        .filter_map(|hit| match &hit.hit {
            SearchHitTarget::Commit { commit } => Some(commit.as_ref()),
            _ => None,
        })
        .collect()
}

/// Each hit's summary line, in answer order.
fn summaries(answer: &SearchResult) -> Vec<&str> {
    commits(answer)
        .into_iter()
        .map(|commit| commit.message.lines().next().unwrap_or_default())
        .collect()
}

#[test]
fn a_commit_search_answers_every_term_first_then_some_each_newest_first() -> TestResult {
    let directory = three_commits()?;
    let folder = tempfile::tempdir()?;
    let service = searchable(directory.path(), folder.path())?;

    let answer = service.search_commits(&commit_search("beacon body")?)?;

    assert_eq!(
        summaries(&answer),
        ["Grow the beacon body", "Introduce the beacon"],
        "the commit carrying both terms comes first: {answer:?}"
    );
    assert!(answer.warnings.is_empty(), "{answer:?}");
    let single = service.search_commits(&commit_search("release")?)?;
    assert_eq!(summaries(&single), ["Fix the release notes"]);
    let none = service.search_commits(&commit_search("nothing matches this")?)?;
    assert!(none.results.is_empty());
    assert_eq!(none.pagination.total_pages, 0);
    Ok(())
}

#[test]
fn a_commit_hit_carries_its_message_author_time_and_changed_paths() -> TestResult {
    let directory = three_commits()?;
    let folder = tempfile::tempdir()?;
    let service = searchable(directory.path(), folder.path())?;

    let answer = service.search_commits(&commit_search("grow")?)?;

    let [commit] = commits(&answer)[..] else {
        return Err(format!("one commit matches: {answer:?}").into());
    };
    let head = rift_history::Repository::open(directory.path())?
        .resolve("HEAD~1")?
        .commit_id();
    assert_eq!(commit.revision.0, head);
    assert_eq!(commit.message, "Grow the beacon body\n");
    assert!(!commit.message_truncated);
    assert_eq!(commit.author.name, "Rift Fixture");
    assert_eq!(commit.author.email, "fixture@rift.invalid");
    assert_eq!(commit.timestamp, "2026-01-01T00:02:00+00:00");
    let paths: Vec<&str> = commit.paths.iter().map(|path| path.0.as_str()).collect();
    assert_eq!(
        paths,
        ["src/lib.rs", "src/ping.rs"],
        "the lockfile the commit changed is not listed"
    );
    assert!(!commit.paths_truncated);
    let hit = &answer.results[0];
    assert!(hit.path.is_none() && hit.unit.is_none() && hit.range.is_none());
    assert!(hit.score.is_none() && hit.matched_by.is_empty());
    Ok(())
}

#[test]
fn a_long_message_and_many_paths_are_cut_and_the_hit_says_so() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    init(root);
    write(root, "seed.txt", "seed\n")?;
    commit_all(root, "seed");
    for index in 0..=COMMIT_PATHS_MAX {
        write(root, &format!("notes/{index:03}.txt"), "note\n")?;
    }
    // A multibyte character straddles the message bound, so the cut lands before it.
    let mut message = "Spread the notes\n\n".to_owned();
    while message.len() < COMMIT_MESSAGE_BYTES_MAX - 1 {
        message.push('x');
    }
    message.push('\u{e9}');
    writeln!(message, " and more")?;
    commit_all(root, &message);
    let folder = tempfile::tempdir()?;
    let service = searchable(root, folder.path())?;

    let answer = service.search_commits(&commit_search("spread")?)?;

    let [commit] = commits(&answer)[..] else {
        return Err(format!("one commit matches: {answer:?}").into());
    };
    assert!(commit.message_truncated);
    assert_eq!(commit.message.len(), COMMIT_MESSAGE_BYTES_MAX - 1);
    assert!(message.starts_with(&commit.message));
    assert!(commit.paths_truncated);
    assert_eq!(commit.paths.len(), COMMIT_PATHS_MAX);
    assert_eq!(commit.paths[0].0, "notes/000.txt");
    Ok(())
}

#[test]
fn a_commit_search_past_the_results_bound_warns_results_truncated() -> TestResult {
    let directory = three_commits()?;
    let folder = tempfile::tempdir()?;
    let history = HistoryConfiguration::default();
    let store = filled_store(directory.path(), folder.path(), &history)?;
    let limits = WorkspaceIndexLimits::new(10_000, 1 << 20, 64 << 20, 64, 1)?;
    let service = service(directory.path(), limits, history, Some(&store))?;

    let answer = service.search_commits(&commit_search("the")?)?;

    assert_eq!(summaries(&answer), ["Fix the release notes"]);
    assert_eq!(
        answer.warnings,
        [ReadWarning::ResultsTruncated { results_max: 1 }]
    );
    Ok(())
}

#[test]
fn a_commit_search_whose_every_term_phase_fills_the_bound_adds_no_broad_match() -> TestResult {
    let directory = three_commits()?;
    let folder = tempfile::tempdir()?;
    let history = HistoryConfiguration::default();
    let store = filled_store(directory.path(), folder.path(), &history)?;
    let limits = WorkspaceIndexLimits::new(10_000, 1 << 20, 64 << 20, 64, 1)?;
    let service = service(directory.path(), limits, history, Some(&store))?;

    // Two commits carry both terms, one past the bound of one, so the release notes
    // commit carrying `the` alone is never read into the answer.
    let answer = service.search_commits(&commit_search("the beacon")?)?;

    assert_eq!(summaries(&answer), ["Grow the beacon body"]);
    assert_eq!(
        answer.warnings,
        [ReadWarning::ResultsTruncated { results_max: 1 }]
    );
    Ok(())
}

#[test]
fn a_commit_trimmed_between_the_match_and_the_read_answers_no_hit() -> TestResult {
    let directory = three_commits()?;
    let folder = tempfile::tempdir()?;
    let history = HistoryConfiguration::default();
    let store = filled_store(directory.path(), folder.path(), &history)?;
    let reads = store.reader().connect()?;
    let matched = reads.search_messages("release", 10)?;
    let mut filler = store.filler()?.ok_or("no other filler runs")?;
    filler.trim(&std::collections::BTreeSet::new())?;

    let hit = commit_hit(&reads, &matched[0])?;

    assert_eq!(matched.len(), 1);
    assert!(hit.is_none(), "{hit:?}");
    Ok(())
}

#[test]
fn a_commit_search_over_a_store_whose_message_index_is_gone_refuses() -> TestResult {
    let directory = three_commits()?;
    let folder = tempfile::tempdir()?;
    let service = searchable(directory.path(), folder.path())?;
    let store_folder = folder.path().join(rift_history_store::STORE_FOLDER_NAME);
    let connection = rusqlite::Connection::open(store_folder.join("store-aa.db"))?;
    connection.execute_batch("DROP TABLE commit_text;")?;

    let refused = service.search_commits(&commit_search("beacon")?);

    let error = refused.expect_err("no message index answers the match");
    assert_eq!(error.slug(), errors::history_store::database::SLUG);
    assert!(
        error
            .context()
            .any(|(key, value)| { key == "operation" && value == "search commit messages" })
    );
    assert!(
        error.source().is_some(),
        "SQLite failure source is retained"
    );
    Ok(())
}

#[test]
fn a_commit_search_pages_by_limit() -> TestResult {
    let directory = three_commits()?;
    let folder = tempfile::tempdir()?;
    let service = searchable(directory.path(), folder.path())?;
    let paged: SearchParams = serde_json::from_value(
        json!({"target": "commit", "query": "beacon", "limit": 1, "page_index": 1}),
    )?;

    let second = service.search_commits(&paged)?;

    assert_eq!(summaries(&second), ["Introduce the beacon"]);
    assert_eq!(second.pagination.total_pages, 2);
    Ok(())
}

#[test]
fn a_selective_store_answers_the_selected_releases_alone() -> TestResult {
    let directory = three_commits()?;
    let root = directory.path();
    git(root, &["tag", "v1.0.0", "HEAD~2"]);
    git(root, &["tag", "v2.0.0", "HEAD"]);
    let folder = tempfile::tempdir()?;
    let history = HistoryConfiguration {
        strategy: HistoryStrategy::Selective,
        releases: vec!["v*".to_owned()],
        ..HistoryConfiguration::default()
    };
    let store = filled_store(root, folder.path(), &history)?;
    let service = service(root, WorkspaceIndexLimits::default(), history, Some(&store))?;

    let answer = service.search_commits(&commit_search("the")?)?;

    assert_eq!(
        summaries(&answer),
        ["Fix the release notes", "Introduce the beacon"],
        "the commit between the two releases is no selected release"
    );
    Ok(())
}

#[test]
fn a_commit_search_refuses_every_field_beside_query() -> TestResult {
    let refused = [
        (json!({"pattern": "beacon"}), "pattern"),
        (
            json!({"traversal": {"seed": "rift://symbol/rust/src/lib.rs/beacon"}}),
            "traversal",
        ),
        (json!({"change": {"base": "HEAD~1"}}), "change"),
        (json!({"rev": "HEAD~1"}), "rev"),
        (
            json!({"packages": [{"manager": "cargo", "name": "tokio", "version": "1.47.1"}]}),
            "packages",
        ),
        (json!({"paths": {"include": ["src/**"]}}), "paths"),
        (json!({"scope": "all"}), "scope"),
        (json!({"scope": "global"}), "scope"),
    ];
    for (extra, field) in refused {
        let mut request = json!({"target": "commit", "query": "beacon"});
        merge(&mut request, &extra);
        let params: SearchParams = serde_json::from_value(request.clone())?;
        let refusal = commit_conflict(&params).ok_or(format!("{request} is refused"))?;
        assert_eq!(
            refusal.slug(),
            rift_error::errors::server::read_invalid::SLUG
        );
        assert!(
            refusal
                .context()
                .any(|(key, value)| key == "field" && value == field),
            "{request}: {refusal}"
        );
    }
    let plain: SearchParams = serde_json::from_value(json!({"query": "beacon", "pattern": "b"}))?;
    assert!(
        commit_conflict(&plain).is_none(),
        "another target answers its own rules"
    );
    Ok(())
}

fn merge(request: &mut Value, extra: &Value) {
    if let (Some(request), Some(extra)) = (request.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            request.insert(key.clone(), value.clone());
        }
    }
}

#[test]
fn a_commit_search_refuses_a_missing_or_empty_query() -> TestResult {
    let directory = three_commits()?;
    let folder = tempfile::tempdir()?;
    let service = searchable(directory.path(), folder.path())?;

    for (request, violation) in [
        (json!({"target": "commit"}), "missing"),
        (json!({"target": "commit", "query": ""}), "empty"),
    ] {
        let params: SearchParams = serde_json::from_value(request.clone())?;
        let Err(refusal) = service.search_commits(&params) else {
            return Err(format!("{request} is refused").into());
        };
        assert_eq!(
            refusal.slug(),
            rift_error::errors::server::read_invalid::SLUG
        );
        assert!(
            refusal
                .context()
                .any(|(key, value)| key == "field" && value == "query")
        );
        assert!(
            refusal
                .context()
                .any(|(key, value)| key == "violation" && value == violation)
        );
    }
    Ok(())
}

#[test]
fn a_commit_search_without_a_store_is_unsupported() -> TestResult {
    let directory = three_commits()?;
    let root = directory.path();
    let unattached = service(
        root,
        WorkspaceIndexLimits::default(),
        HistoryConfiguration::default(),
        None,
    )?;
    let disabled = service(
        root,
        WorkspaceIndexLimits::default(),
        HistoryConfiguration {
            enabled: false,
            ..HistoryConfiguration::default()
        },
        None,
    )?;

    for (service, detail) in [
        (unattached, "commit search"),
        (disabled, "commit search (providers.history disabled)"),
    ] {
        let Err(refusal) = service.search_commits(&commit_search("beacon")?) else {
            return Err("a commit search needs the history store".into());
        };
        assert_eq!(refusal.slug(), errors::server::read_unsupported::SLUG);
        assert!(
            refusal
                .context()
                .any(|(key, value)| { key == "capability" && value == detail })
        );
    }
    Ok(())
}

#[test]
fn search_routes_a_commit_target_to_the_history_store() -> TestResult {
    let directory = three_commits()?;
    let folder = tempfile::tempdir()?;
    let service = searchable(directory.path(), folder.path())?;

    let answer = service.search(&commit_search("release")?, &super::StoreAnswer::default())?;

    assert_eq!(summaries(&answer), ["Fix the release notes"]);
    Ok(())
}

#[test]
fn a_commit_query_past_the_member_bound_narrows_and_warns() -> TestResult {
    let directory = three_commits()?;
    let folder = tempfile::tempdir()?;
    let service = searchable(directory.path(), folder.path())?;
    let mut query = "release".to_owned();
    for index in 0..40 {
        write!(query, " filler{index:02}")?;
    }

    let answer = service.search_commits(&commit_search(&query)?)?;

    assert!(
        answer
            .warnings
            .iter()
            .any(|warning| matches!(warning, ReadWarning::QueryNarrowed { .. })),
        "{answer:?}"
    );
    Ok(())
}

/// A store under `folder` holding the newest `written` of the commits the default table
/// selects in the workspace at `root`, a snapshot it is attached to, and the count of reads
/// that asked for a fill. The progress records the fill plan and the written batch the way the
/// history task does.
fn partly_filled(
    root: &Path,
    folder: &Path,
    written: usize,
) -> TestResult<(HistoryStore, ReadService, Arc<AtomicUsize>)> {
    let history = HistoryConfiguration::default();
    let store = HistoryStore::open(&StoreLocation::new(folder, "aa"))?;
    let mut filler = store.filler()?.ok_or("no other filler runs")?;
    let analysis = HistoryAnalysis::open(
        root,
        &history,
        (
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &LanguageFileSelections::default(),
        ),
        SyntaxLimits::default(),
    )
    .map_err(|error| error.to_string())?;
    let plan = analysis
        .plan(&filler.held()?)
        .map_err(|error| error.to_string())?;
    let mut records = Vec::new();
    for pending in plan.pending().iter().take(written) {
        let analyzed = analysis
            .analyze(pending, &|| false)
            .map_err(|error| error.to_string())?
            .ok_or("nothing cancels the analysis")?;
        records.push(analyzed.into_record());
    }
    filler.write_batch(&records)?;
    let lagged = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&lagged);
    let stored = StoredHistory::new(
        store.reader(),
        history.strategy,
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let progress = stored.progress();
    progress.record_plan(plan.keep().len(), plan.pending().len());
    progress.record_written(records.len());
    let service = ReadService::build(
        root,
        WorkspaceIndexLimits::default(),
        &SourceVisibility::default(),
        &TextFileInclusion::default(),
        history,
    )?;
    service.attach_history_store(&stored);
    Ok((store, service, lagged))
}

/// The filling warning an answer carries, as its two counts.
fn filling_counts(answer: &SearchResult) -> Vec<(u64, u64)> {
    answer
        .warnings
        .iter()
        .filter_map(|warning| match warning {
            ReadWarning::HistoryStoreFilling {
                analyzed, total, ..
            } => Some((*analyzed, *total)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_partly_filled_store_answers_what_it_holds_and_says_so() -> TestResult {
    let directory = three_commits()?;
    let folder = tempfile::tempdir()?;
    let (_store, service, lagged) = partly_filled(directory.path(), folder.path(), 1)?;

    let answer = service.search_commits(&commit_search("the")?)?;

    assert_eq!(summaries(&answer), ["Fix the release notes"]);
    assert_eq!(filling_counts(&answer), [(1, 3)], "{answer:?}");
    assert_eq!(lagged.load(Ordering::SeqCst), 0, "the store holds HEAD");
    Ok(())
}

#[test]
fn a_filled_store_carries_no_filling_warning() -> TestResult {
    let directory = three_commits()?;
    let folder = tempfile::tempdir()?;
    let (_store, service, _) = partly_filled(directory.path(), folder.path(), 3)?;

    let answer = service.search_commits(&commit_search("the")?)?;

    assert_eq!(summaries(&answer).len(), 3);
    assert!(filling_counts(&answer).is_empty(), "{answer:?}");
    Ok(())
}

#[test]
fn a_store_missing_the_checked_out_head_says_so_and_asks_for_a_fill() -> TestResult {
    let directory = three_commits()?;
    let folder = tempfile::tempdir()?;
    let (_store, service, lagged) = partly_filled(directory.path(), folder.path(), 3)?;
    write(directory.path(), "NOTES.md", "# Notes\n\nMore.\n")?;
    commit_all_at(
        directory.path(),
        "Extend the release notes\n",
        "2026-01-01T00:04:00 +0000",
    );

    let answer = service.search_commits(&commit_search("release")?)?;

    assert_eq!(summaries(&answer), ["Fix the release notes"]);
    assert_eq!(filling_counts(&answer), [(3, 3)], "{answer:?}");
    let detail = answer
        .warnings
        .iter()
        .find_map(|warning| match warning {
            ReadWarning::HistoryStoreFilling { detail, .. } => Some(detail.as_str()),
            _ => None,
        })
        .ok_or("the answer says the store is filling")?;
    assert!(detail.contains("HEAD"), "{detail}");
    assert_eq!(lagged.load(Ordering::SeqCst), 1, "the read asks for a fill");
    Ok(())
}

#[test]
fn fill_progress_counts_what_the_latest_plan_still_owes() {
    let progress = FillProgress::default();
    assert_eq!(progress.counts(), None, "no plan has landed");
    progress.record_written(5);
    assert_eq!(
        progress.counts(),
        None,
        "a write before any plan records nothing"
    );

    progress.record_plan(10, 4);
    let counts = progress.counts().expect("a plan landed");
    assert_eq!((counts.analyzed(), counts.total()), (6, 10));
    progress.record_written(3);
    progress.record_written(3);
    let counts = progress.counts().expect("a plan landed");
    assert_eq!(
        (counts.analyzed(), counts.total()),
        (10, 10),
        "a write past what the fill plan owed saturates"
    );
    progress.record_plan(2, 9);
    let counts = progress.counts().expect("a plan landed");
    assert_eq!(
        (counts.analyzed(), counts.total()),
        (0, 2),
        "a plan never owes more than it selects"
    );
}
