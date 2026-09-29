use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rift_history::fixture::{commit_all, git, init};
use rift_history_store::{HistoryStore, StoreLocation};
use rift_protocol::configuration::HistoryConfiguration;
use tokio_util::sync::CancellationToken;

use super::{FillBounds, HistoryLane, OpenedStore, store_revision};
use crate::http::IdleTracker;
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
