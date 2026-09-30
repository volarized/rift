use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rift_history::fixture::{commit_all, commit_missing_subtree, git, init};
use rift_history_store::{CommitRecord, HistoryStore, STORE_FOLDER_NAME, StoreLocation};
use rift_protocol::configuration::HistoryConfiguration;
use rift_protocol::read::CommitAuthor;
use rift_server::FillProgress;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::layer::SubscriberExt as _;

use super::{AnalysisGate, FillBounds, HistoryLane, HistoryTask, OpenedStore, store_revision};
use crate::http::IdleTracker;
use crate::logs::{LogDrain, log_capture};
use crate::validation::ConfigurationState;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// How long a test waits for a background fill of a few fixture commits.
const FILL_WAIT_MAX: Duration = Duration::from_secs(30);

/// The poll interval a test re-reads the store at while it waits.
const FILL_POLL: Duration = Duration::from_millis(50);

/// A committed workspace: `beacon` introduced, then grown, under `rift_toml`.
fn committed_workspace(rift_toml: &str) -> TestResult<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    crate::server::hermetic_workspace(root, rift_toml)?;
    fs::write(root.join("lib.rs"), "pub fn beacon() {}\n")?;
    init(root);
    commit_all(root, "introduce beacon");
    fs::write(root.join("lib.rs"), "pub fn beacon() { let _grown = 1; }\n")?;
    commit_all(root, "grow beacon");
    Ok(directory)
}

fn head_of(root: &Path) -> TestResult<String> {
    Ok(rift_history::Repository::open(root)?
        .resolve("HEAD")?
        .commit_id())
}

/// The store a lane over `root` opens, for a test that reads it directly.
fn lane_store(root: &Path, digest: &str) -> TestResult<HistoryStore> {
    let configuration = ConfigurationState::accept(root);
    let revision = store_revision(digest, &configuration);
    Ok(HistoryStore::open(&StoreLocation::new(
        &root.join(".git"),
        &revision,
    ))?)
}

/// Waits until the store at `root` holds `commit`, at most [`FILL_WAIT_MAX`].
async fn wait_until_held(root: &Path, digest: &str, commit: &str) -> TestResult {
    let store = lane_store(root, digest)?;
    let reader = store.reader();
    let deadline = tokio::time::Instant::now() + FILL_WAIT_MAX;
    loop {
        if reader.connect()?.commit(commit)?.is_some() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("the store never held {commit}").into());
        }
        tokio::time::sleep(FILL_POLL).await;
    }
}

/// The history task over the store a lane opens for `root`, driven one step at a
/// time, with `gate` run before each commit's analysis.
fn history_task(root: &Path, gate: Option<AnalysisGate>) -> TestResult<HistoryTask> {
    let configuration = ConfigurationState::accept(root);
    let history = configuration.history_configuration();
    let opened = OpenedStore::open(root, &configuration, &history, "executable-a")()
        .ok_or("a committed workspace opens its history store")?;
    Ok(HistoryTask {
        store: Arc::new(opened.store),
        analysis: Arc::new(opened.analysis),
        progress: Arc::new(FillProgress::default()),
        bounds: FillBounds::from_configuration(&history),
        activity: Arc::new(IdleTracker::new()),
        warned_tags: HashSet::new(),
        warned_past_bound: 0,
        gate,
    })
}

/// How many commits the store of `task` holds.
fn held(task: &HistoryTask) -> TestResult<usize> {
    Ok(task.store.reader().connect()?.held()?.len())
}

/// The file `suffix` names beside the store `task` opened.
fn store_file(task: &HistoryTask, suffix: &str) -> PathBuf {
    let location = task.store.location();
    let revision = location.revision();
    location.folder().join(format!("store-{revision}{suffix}"))
}

/// Runs `statements` on the store `task` opened, through a connection of its own.
fn execute_on_store(task: &HistoryTask, statements: &str) -> TestResult {
    let connection = rusqlite::Connection::open(store_file(task, ".db"))?;
    connection.execute_batch(statements)?;
    Ok(())
}

/// The message and fields of each record `drain` holds at `operation`.
fn records_at(drain: &mut LogDrain, operation: &str) -> Vec<(String, String)> {
    let mut records = Vec::new();
    while let Ok(record) = drain.try_recv_record() {
        if record.operation() == operation {
            records.push((record.message().to_owned(), record.fields().to_owned()));
        }
    }
    records
}

/// The store the lane over `root` opens, opened under a log capture.
fn opened_under_capture(root: &Path) -> (Option<OpenedStore>, LogDrain) {
    let configuration = ConfigurationState::accept(root);
    let history = configuration.history_configuration();
    let (sink, drain) = log_capture();
    let subscriber = tracing_subscriber::registry().with(sink);
    let opened = tracing::subscriber::with_default(subscriber, || {
        OpenedStore::open(root, &configuration, &history, "executable-a")()
    });
    (opened, drain)
}

async fn start(
    root: &Path,
    digest: &str,
    activity: Arc<IdleTracker>,
    cancellation: &CancellationToken,
) -> TestResult<HistoryLane> {
    let configuration = ConfigurationState::accept(root);
    HistoryLane::start(
        root,
        &configuration,
        digest,
        (activity, cancellation.clone(), None),
    )
    .await
    .ok_or_else(|| "a committed workspace opens its history store".into())
}

#[test]
fn a_batch_takes_its_first_commit_whatever_it_parsed_and_more_within_both_bounds() {
    let bounds = FillBounds::from_configuration(&HistoryConfiguration::default());
    assert!(bounds.admits(0, 0, 5_000_000), "one commit is the floor");
    assert!(bounds.admits(1, 400_000, 600_000));
    assert!(
        !bounds.admits(1, 400_000, 600_001),
        "no batch holds more than 1,000,000 parsed bytes past its first commit"
    );
    assert!(bounds.admits(24, 0, 0));
    assert!(
        !bounds.admits(25, 0, 0),
        "no batch holds more than 25 commits"
    );
    assert!(
        !bounds.admits(1, 5_000_000, 0),
        "an oversized first commit is a batch alone"
    );
}

#[test]
fn the_pause_after_a_parse_holds_the_cpu_share() {
    let share = |cpu_share: f64| {
        FillBounds::from_configuration(&HistoryConfiguration {
            cpu_share,
            ..HistoryConfiguration::default()
        })
    };
    let spent = Duration::from_millis(100);
    assert_eq!(share(0.25).pause_after(spent), Duration::from_millis(300));
    assert_eq!(share(0.5).pause_after(spent), spent);
    assert_eq!(share(1.0).pause_after(spent), Duration::ZERO);
    assert_eq!(share(0.25).pause_after(Duration::ZERO), Duration::ZERO);
}

#[test]
fn two_builds_and_two_strategies_key_two_store_files() -> TestResult {
    let everything = committed_workspace("")?;
    let selective = committed_workspace(
        "[providers.history]\nstrategy = \"selective\"\nreleases = [\"v*\"]\n",
    )?;
    let everything = ConfigurationState::accept(everything.path());
    let selective = ConfigurationState::accept(selective.path());

    let build_a = store_revision("executable-a", &everything);

    assert_eq!(build_a, store_revision("executable-a", &everything));
    assert_ne!(build_a, store_revision("executable-b", &everything));
    assert_ne!(build_a, store_revision("executable-a", &selective));
    Ok(())
}

#[tokio::test]
async fn a_lane_fills_the_store_in_the_background_and_each_build_its_own_file() -> TestResult {
    let directory = committed_workspace("")?;
    let root = directory.path();
    let head = head_of(root)?;
    let cancellation = CancellationToken::new();

    let _first = start(
        root,
        "executable-a",
        Arc::new(IdleTracker::new()),
        &cancellation,
    )
    .await?;
    let _second = start(
        root,
        "executable-b",
        Arc::new(IdleTracker::new()),
        &cancellation,
    )
    .await?;
    wait_until_held(root, "executable-a", &head).await?;
    wait_until_held(root, "executable-b", &head).await?;

    let files: Vec<String> = fs::read_dir(root.join(".git/rift"))?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_, _>>()?;
    let databases = files
        .iter()
        .filter(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|extension| extension == "db")
        })
        .count();
    assert_eq!(
        databases, 2,
        "each build fills its own store file: {files:?}"
    );
    let in_worktree = fs::read_dir(root.join(".rift")).is_ok_and(|mut entries| {
        entries
            .by_ref()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with("store-"))
    });
    assert!(
        !in_worktree,
        "a writable common git directory keeps the store there"
    );
    cancellation.cancel();
    Ok(())
}

#[tokio::test]
async fn a_selective_lane_fills_the_releases_it_selects() -> TestResult {
    let directory = committed_workspace(
        "[providers.history]\nstrategy = \"selective\"\nreleases = [\"v*\"]\n",
    )?;
    let root = directory.path();
    git(root, &["tag", "v1.0.0", "HEAD~1"]);
    git(root, &["tag", "v2.0.0", "HEAD"]);
    let head = head_of(root)?;
    let cancellation = CancellationToken::new();

    let _lane = start(
        root,
        "executable-a",
        Arc::new(IdleTracker::new()),
        &cancellation,
    )
    .await?;
    wait_until_held(root, "executable-a", &head).await?;

    let reads = lane_store(root, "executable-a")?.reader().connect()?;
    let newest = reads.commit(&head)?.ok_or("the newest release is held")?;
    let oldest = newest.base.clone().ok_or("the newest release has a base")?;
    let oldest = reads.commit(&oldest)?.ok_or("the oldest release is held")?;
    assert!(
        oldest.boundary,
        "the oldest release is compared with nothing"
    );
    cancellation.cancel();
    Ok(())
}

#[tokio::test]
async fn the_fill_runs_a_batch_after_its_bounded_wait_while_requests_overlap() -> TestResult {
    let directory = committed_workspace("")?;
    let root = directory.path();
    let head = head_of(root)?;
    let activity = Arc::new(IdleTracker::new());
    let busy = activity.begin();
    let cancellation = CancellationToken::new();

    let _lane = start(root, "executable-a", Arc::clone(&activity), &cancellation).await?;
    wait_until_held(root, "executable-a", &head).await?;

    drop(busy);
    cancellation.cancel();
    Ok(())
}

#[tokio::test]
async fn a_batch_records_its_start_with_the_pending_commits_of_the_plan() -> TestResult {
    use tracing_subscriber::layer::SubscriberExt as _;

    let directory = committed_workspace("")?;
    let root = directory.path();
    let head = head_of(root)?;
    let (sink, mut drain) = crate::logs::log_capture();
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry().with(sink));
    let cancellation = CancellationToken::new();
    let activity = Arc::new(IdleTracker::new());

    let _lane = start(root, "executable-a", activity, &cancellation).await?;
    wait_until_held(root, "executable-a", &head).await?;
    cancellation.cancel();

    let mut starts = Vec::new();
    while let Ok(record) = drain.try_recv_record() {
        if record.message() == "history batch started" {
            starts.push(record);
        }
    }
    let first = starts.first().ok_or("no batch start was recorded")?;
    assert_eq!(first.level(), "debug");
    assert_eq!(first.component(), "history");
    assert_eq!(first.operation(), "history.batch");
    assert_eq!(
        first.fields(),
        r#"{"phase":"start","pending":"2"}"#,
        "the first batch starts with both commits of the fixture pending"
    );
    Ok(())
}

#[tokio::test]
async fn a_settled_wait_ends_at_its_bound_or_when_the_last_request_completes() {
    let activity = IdleTracker::new();
    assert!(activity.settled(Duration::from_millis(10)).await);

    let busy = activity.begin();
    assert!(!activity.settled(Duration::from_millis(20)).await);

    let waiting = activity.settled(Duration::from_secs(30));
    tokio::pin!(waiting);
    tokio::select! {
        biased;
        _ = &mut waiting => panic!("the wait cannot settle while a request runs"),
        () = tokio::time::sleep(Duration::from_millis(20)) => {}
    }
    drop(busy);
    assert!(waiting.await, "the last completion settles the wait");
}

#[cfg(unix)]
#[test]
fn a_read_only_common_git_directory_keeps_the_store_in_the_worktree_and_warns_once() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    let directory = committed_workspace("")?;
    let root = directory.path();
    fs::create_dir_all(root.join(".rift"))?;
    let configuration = ConfigurationState::accept(root);
    let history = configuration.history_configuration();
    fs::set_permissions(root.join(".git"), fs::Permissions::from_mode(0o555))?;
    let (sink, mut drain) = crate::logs::log_capture();
    let subscriber = tracing_subscriber::registry().with(sink);
    let opened = tracing::subscriber::with_default(subscriber, || {
        OpenedStore::open(root, &configuration, &history, "executable-a")()
    });
    fs::set_permissions(root.join(".git"), fs::Permissions::from_mode(0o755))?;

    let opened = opened.ok_or("the worktree's state directory takes the store")?;
    assert_eq!(opened.store.location().folder(), root.join(".rift"));
    assert!(!root.join(".git/rift").exists());
    let recorded = drain.try_recv_record().map_err(|error| error.to_string())?;
    assert_eq!(recorded.level(), "warn");
    assert_eq!(recorded.operation(), "history.open");
    assert!(
        drain.try_recv_record().is_err(),
        "the fallback is recorded once, when the server opens its store"
    );
    Ok(())
}

#[tokio::test]
async fn a_disabled_history_provider_opens_no_store() -> TestResult {
    let directory = committed_workspace("[providers.history]\nenabled = false\n")?;
    let configuration = ConfigurationState::accept(directory.path());

    let lane = HistoryLane::start(
        directory.path(),
        &configuration,
        "executable-a",
        (Arc::new(IdleTracker::new()), CancellationToken::new(), None),
    )
    .await;

    assert!(lane.is_none());
    assert!(!directory.path().join(".git/rift").exists());
    Ok(())
}

#[tokio::test]
async fn an_unversioned_workspace_opens_no_store() -> TestResult {
    let directory = tempfile::tempdir()?;
    crate::server::hermetic_workspace(directory.path(), "")?;
    let configuration = ConfigurationState::accept(directory.path());

    let lane = HistoryLane::start(
        directory.path(),
        &configuration,
        "executable-a",
        (Arc::new(IdleTracker::new()), CancellationToken::new(), None),
    )
    .await;

    assert!(lane.is_none());
    Ok(())
}

/// The message a lane logs when its store does not open.
const STORE_NOT_OPENED: &str =
    "the history store could not open; symbol history walks git per request";

/// The message a failed fill step logs.
const FILL_STOPPED: &str =
    "a history store fill stopped; the next fill plans again from what the store holds";

#[test]
fn a_store_folder_the_filesystem_cannot_create_leaves_the_lane_off_and_warns() -> TestResult {
    let directory = committed_workspace("")?;
    let root = directory.path();
    fs::write(
        root.join(".git").join(STORE_FOLDER_NAME),
        b"a file where the folder goes\n",
    )?;

    let (opened, mut drain) = opened_under_capture(root);

    assert!(opened.is_none());
    let records = records_at(&mut drain, "history.open");
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].0, STORE_NOT_OPENED);
    assert!(records[0].1.contains("create store folder"), "{records:?}");
    Ok(())
}

#[test]
fn a_release_pattern_no_glob_compiles_from_leaves_the_lane_off_and_warns() -> TestResult {
    let directory = committed_workspace(
        "[providers.history]\nstrategy = \"selective\"\nreleases = [\"v[1\"]\n",
    )?;
    let root = directory.path();
    assert!(ConfigurationState::accept(root).is_accepted());

    let (opened, mut drain) = opened_under_capture(root);

    assert!(opened.is_none());
    let records = records_at(&mut drain, "history.open");
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].0, STORE_NOT_OPENED);
    assert!(
        records[0].1.contains("providers.history.releases"),
        "{records:?}"
    );
    Ok(())
}

#[test]
fn a_released_store_file_the_sweep_cannot_delete_is_logged_and_the_lane_opens() -> TestResult {
    let directory = committed_workspace("")?;
    let root = directory.path();
    let folder = root.join(".git").join(STORE_FOLDER_NAME);
    fs::create_dir_all(folder.join("store-released.db"))?;
    fs::write(folder.join("store-released.live.lock"), b"")?;

    let (opened, mut drain) = opened_under_capture(root);

    assert!(opened.is_some());
    let records = records_at(&mut drain, "history.sweep");
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        records[0].0,
        "a released history store file could not be deleted"
    );
    assert!(folder.join("store-released.live.lock").exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_store_folder_the_sweep_cannot_list_is_logged_and_the_lane_opens() -> TestResult {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = committed_workspace("")?;
    let root = directory.path();
    let folder = root.join(".git").join(STORE_FOLDER_NAME);
    fs::create_dir_all(&folder)?;
    // Creating and opening files in the folder needs write and search access alone;
    // listing it needs read access.
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o300))?;
    let (opened, mut drain) = opened_under_capture(root);
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o755))?;

    assert!(opened.is_some());
    let records = records_at(&mut drain, "history.sweep");
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].0, "the history store folder could not be swept");
    Ok(())
}

#[tokio::test]
async fn a_plan_that_fails_is_logged_whether_the_task_fills_or_observes() -> TestResult {
    let directory = committed_workspace("")?;
    let root = directory.path();
    // Every plan reads the shallow file, and a folder in its place refuses the read.
    fs::create_dir(root.join(".git/shallow"))?;
    let (sink, mut drain) = log_capture();
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry().with(sink));
    let mut task = history_task(root, None)?;
    let cancellation = CancellationToken::new();

    let filler = task.fill(None, &cancellation).await;
    assert!(filler.is_some(), "a failed plan keeps the fill lock");
    drop(filler);
    let _other = task.store.filler()?.ok_or("the fill lock is free again")?;
    let observed = task.fill(None, &cancellation).await;

    assert!(observed.is_none(), "another filler holds the fill lock");
    let records = records_at(&mut drain, "history.fill");
    assert_eq!(
        records.len(),
        3,
        "the open, the fill, and the observation: {records:?}"
    );
    assert!(records.iter().all(|(message, _)| message == FILL_STOPPED));
    assert!(records[2].1.contains("read shallow file"), "{records:?}");
    Ok(())
}

#[tokio::test]
async fn a_fill_lock_the_filesystem_refuses_is_logged_and_the_task_observes() -> TestResult {
    let directory = committed_workspace("")?;
    let (sink, mut drain) = log_capture();
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry().with(sink));
    let mut task = history_task(directory.path(), None)?;
    fs::create_dir(store_file(&task, ".fill.lock"))?;

    let filler = task.fill(None, &CancellationToken::new()).await;

    assert!(filler.is_none());
    let records = records_at(&mut drain, "history.fill");
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(records[0].1.contains("open fill lock"), "{records:?}");
    let counts = task
        .progress
        .counts()
        .ok_or("the observed plan is recorded")?;
    assert_eq!((counts.analyzed(), counts.total()), (0, 2));
    Ok(())
}

#[tokio::test]
async fn a_commit_past_the_batch_bound_opens_the_next_batch() -> TestResult {
    let directory = committed_workspace("")?;
    let mut task = history_task(directory.path(), None)?;
    task.bounds.commits_max = 1;

    let filler = task.fill(None, &CancellationToken::new()).await;

    assert!(filler.is_some());
    assert_eq!(held(&task)?, 2, "each commit is a batch of its own");
    let counts = task.progress.counts().ok_or("the plan is recorded")?;
    assert_eq!((counts.analyzed(), counts.total()), (2, 2));
    Ok(())
}

#[tokio::test]
async fn a_stop_before_the_first_batch_writes_nothing() -> TestResult {
    let directory = committed_workspace("")?;
    let mut task = history_task(directory.path(), None)?;
    let stopped = CancellationToken::new();
    stopped.cancel();

    let filler = task.fill(None, &stopped).await;

    assert!(filler.is_some(), "a stop hands the fill lock back");
    assert_eq!(held(&task)?, 0);
    Ok(())
}

#[tokio::test]
async fn a_stop_during_the_wait_for_an_idle_server_writes_nothing() -> TestResult {
    let directory = committed_workspace("")?;
    let mut task = history_task(directory.path(), None)?;
    task.bounds.idle_wait = Duration::from_secs(3_600);
    let _busy = task.activity.begin();
    let filler = task.store.filler()?.ok_or("no other filler runs")?;
    let plan = task.analysis.plan(&filler.held()?)?;
    let cancellation = CancellationToken::new();

    // The fill is polled into its wait before the second branch's second poll cancels.
    let (filler, ()) = tokio::join!(task.fill_planned(filler, &plan, &cancellation), async {
        tokio::task::yield_now().await;
        cancellation.cancel();
    });

    assert!(filler.is_some(), "a stop hands the fill lock back");
    assert_eq!(held(&task)?, 0);
    Ok(())
}

#[tokio::test]
async fn a_batch_the_store_refuses_is_logged_and_writes_nothing() -> TestResult {
    let directory = committed_workspace("")?;
    let (sink, mut drain) = log_capture();
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry().with(sink));
    let mut task = history_task(directory.path(), None)?;
    execute_on_store(
        &task,
        "CREATE TRIGGER refuse_commits BEFORE INSERT ON commits \
         BEGIN SELECT RAISE(ABORT, 'the store refuses the batch'); END;",
    )?;

    let filler = task.fill(None, &CancellationToken::new()).await;

    assert!(filler.is_some(), "a refused batch keeps the fill lock");
    assert_eq!(held(&task)?, 0);
    let records = records_at(&mut drain, "history.fill");
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(
        records[0].1.contains("the store refuses the batch"),
        "{records:?}"
    );
    Ok(())
}

/// A commit no window of the fixture selects, for a trim to delete.
fn unselected_commit() -> CommitRecord {
    CommitRecord {
        id: "0000000000000000000000000000000000000001".to_owned(),
        base: None,
        boundary: true,
        author: CommitAuthor {
            name: "Rift Fixture".to_owned(),
            email: "fixture@rift.invalid".to_owned(),
        },
        committed_at: "2026-01-01T00:00:00+00:00".to_owned(),
        time: 1_767_225_600,
        message: "unselected\n".to_owned(),
        paths: Vec::new(),
        renames: Vec::new(),
        moves: Vec::new(),
        declarations: Vec::new(),
    }
}

#[tokio::test]
async fn a_trim_the_store_refuses_is_logged_and_keeps_the_commit() -> TestResult {
    let directory = committed_workspace("")?;
    let (sink, mut drain) = log_capture();
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry().with(sink));
    let mut task = history_task(directory.path(), None)?;
    let mut filler = task.store.filler()?.ok_or("no other filler runs")?;
    filler.write_batch(&[unselected_commit()])?;
    drop(filler);
    execute_on_store(
        &task,
        "CREATE TRIGGER keep_commits BEFORE DELETE ON commits \
         BEGIN SELECT RAISE(ABORT, 'the store refuses the trim'); END;",
    )?;

    let filler = task.fill(None, &CancellationToken::new()).await;

    assert!(filler.is_some());
    assert_eq!(
        held(&task)?,
        3,
        "both fixture commits beside the one the trim kept"
    );
    let records = records_at(&mut drain, "history.fill");
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(
        records[0].1.contains("the store refuses the trim"),
        "{records:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_commit_whose_analysis_fails_ends_the_fill_and_is_logged() -> TestResult {
    let directory = committed_workspace("")?;
    let root = directory.path();
    // `main` moves to a commit whose tree names a folder the object store lacks.
    commit_missing_subtree(root, "refs/heads/main");
    let (sink, mut drain) = log_capture();
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry().with(sink));
    let mut task = history_task(root, None)?;

    let filler = task.fill(None, &CancellationToken::new()).await;

    assert!(filler.is_some());
    assert_eq!(held(&task)?, 0);
    let records = records_at(&mut drain, "history.fill");
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(records[0].1.contains("compare commit trees"), "{records:?}");
    Ok(())
}

#[tokio::test]
async fn a_panicking_analysis_ends_the_fill_and_is_logged() -> TestResult {
    let directory = committed_workspace("")?;
    let (sink, mut drain) = log_capture();
    let _subscriber = tracing::subscriber::set_default(tracing_subscriber::registry().with(sink));
    let gate: AnalysisGate = Arc::new(|| panic!("the analysis thread panics"));
    let mut task = history_task(directory.path(), Some(gate))?;

    let filler = task.fill(None, &CancellationToken::new()).await;

    assert!(
        filler.is_some(),
        "the panic took no step that held the fill lock"
    );
    assert_eq!(held(&task)?, 0);
    let records = records_at(&mut drain, "history.fill");
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(records[0].1.contains("panicked"), "{records:?}");
    Ok(())
}

#[test]
fn an_unversioned_release_tag_warns_once_and_the_bound_warns_when_its_count_changes() -> TestResult
{
    let directory = committed_workspace(
        "[providers.history]\nstrategy = \"selective\"\nreleases = [\"v*\"]\nmax_revisions = 1\n",
    )?;
    let root = directory.path();
    git(root, &["tag", "v1.0.0", "HEAD~1"]);
    git(root, &["tag", "v2.0.0", "HEAD"]);
    git(root, &["tag", "vnext", "HEAD"]);
    let (sink, mut drain) = log_capture();
    let subscriber = tracing_subscriber::registry().with(sink);

    tracing::subscriber::with_default(subscriber, || -> TestResult {
        let mut task = history_task(root, None)?;
        let plan = task.analysis.plan(&HashMap::new())?;
        task.record_releases(&plan);
        task.record_releases(&plan);
        Ok(())
    })?;

    let records = records_at(&mut drain, "history.plan");
    let messages: Vec<&str> = records
        .iter()
        .map(|(message, _)| message.as_str())
        .collect();
    assert_eq!(
        messages,
        [
            "a release tag holds no version once the pattern's literal text is stripped, so \
             the history store leaves it out",
            "the release patterns select more releases than max_revisions, so the history \
             store keeps the newest",
        ]
    );
    assert!(records[0].1.contains("vnext"), "{records:?}");
    Ok(())
}
