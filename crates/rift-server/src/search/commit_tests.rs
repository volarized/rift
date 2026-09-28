use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use rift_core::{LanguageFileSelections, SourceVisibility, TextFileInclusion};
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

use super::commit::commit_conflict;
use crate::HistoryAnalysis;
use crate::history::StoredHistory;
use crate::read::{ReadFault, ReadService};

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
        let ReadFault::Invalid { field: named, .. } = refusal.fault() else {
            return Err(format!("expected invalid_request, found {refusal}").into());
        };
        assert_eq!(*named, field, "{request}: {refusal}");
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
        let ReadFault::Invalid {
            field,
            violation: named,
        } = refusal.fault()
        else {
            return Err(format!("expected invalid_request, found {refusal}").into());
        };
        assert_eq!((*field, named.as_str()), ("query", violation), "{request}");
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
        (disabled, "providers.history disabled"),
    ] {
        let Err(refusal) = service.search_commits(&commit_search("beacon")?) else {
            return Err("a commit search needs the history store".into());
        };
        assert_eq!(
            refusal.name(),
            rift_core::ErrorName::Wire(rift_core::ErrorCode::CapabilityUnavailable),
            "{refusal}"
        );
        assert!(refusal.to_string().contains(detail), "{refusal}");
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
