//! Election and lock-file plumbing for one workspace's rift server.
//!
//! Two files under `<root>/.rift` carry the election. `server.lock` is a
//! zero-byte file the serving process holds an exclusive advisory lock on
//! for its whole life; `server.json` is the published [`ServerLock`]
//! document, always written through a temp file and an atomic rename so a
//! reader never observes a partial document. The election lock lives on a
//! file nobody reads because a Windows `LockFileEx` over the data file
//! would block other processes' reads of its content.

use std::fs::{OpenOptions, TryLockError};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rift_core::constants::RIFT_STATE_DIRECTORY;
use rift_core::{CliCode, Error, ErrorCode, ErrorContext, ErrorName, Fault};
use rift_protocol::lock::{SERVER_LOCK_FILE_NAME, ServerLock, ServerLockViolation};
use tokio_util::sync::CancellationToken;

use crate::http::{HttpServeError, HttpServer, serve_http};

/// The zero-byte election file's name under the `.rift` state directory.
///
/// The protocol crate owns only `server.json`'s name; the election file is
/// this module's implementation detail.
const SERVER_ELECTION_FILE_NAME: &str = "server.lock";

/// Longest wait for a server that failed to publish to shut down again.
const UNPUBLISHED_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(10);

/// Failure while claiming, publishing, or serving a workspace's election.
pub type ElectionError = Error<ElectionFault>;

/// One election failure: what stopped this process from serving the
/// workspace.
#[derive(Debug)]
pub enum ElectionFault {
    /// Another process holds this workspace's election lock.
    AlreadyServing,
    /// A lock-file read or write failed.
    Storage {
        /// The filesystem operation that failed.
        operation: &'static str,
        /// The path the operation addressed.
        path: PathBuf,
        /// The underlying failure.
        source: io::Error,
    },
    /// The document this server built breaks the published lock contract.
    DocumentInvalid(ServerLockViolation),
    /// The HTTP transport failed while starting or serving under the
    /// election. Boxed to keep this fault small beside it.
    Serve(Box<HttpServeError>),
}

impl Fault for ElectionFault {
    fn name(&self) -> ErrorName {
        match self {
            Self::AlreadyServing => ErrorName::Cli(CliCode::ServerAlreadyServing),
            Self::Storage { .. } => ErrorName::Wire(ErrorCode::StorageFailure),
            Self::DocumentInvalid(_) => ErrorName::Wire(ErrorCode::InternalError),
            Self::Serve(source) => source.name(),
        }
    }

    fn context(&self) -> Vec<ErrorContext> {
        match self {
            Self::AlreadyServing => Vec::new(),
            Self::Storage {
                operation, path, ..
            } => vec![
                ErrorContext::new("operation", *operation),
                ErrorContext::new("path", path.display().to_string()),
            ],
            Self::DocumentInvalid(violation) => violation
                .evidence()
                .into_iter()
                .map(|(key, value)| ErrorContext::new(key, value))
                .collect(),
            Self::Serve(source) => source.context(),
        }
    }

    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AlreadyServing | Self::DocumentInvalid(_) => None,
            Self::Storage { source, .. } => Some(source),
            Self::Serve(source) => Some(source.as_ref()),
        }
    }
}

impl ElectionFault {
    fn storage(operation: &'static str, path: &Path, source: io::Error) -> ElectionError {
        Error::new(Self::Storage {
            operation,
            path: path.to_owned(),
            source,
        })
    }

    fn serve(source: HttpServeError) -> ElectionError {
        Error::new(Self::Serve(Box::new(source)))
    }
}

/// Claims the workspace's election for this process.
///
/// Creates the `.rift` state directory when absent, opens or creates the
/// election file, and takes its exclusive advisory lock without blocking.
/// The lock is held for the guard's lifetime and releases with the file
/// handle, so a crashed holder never leaves a held election behind.
///
/// # Errors
///
/// Returns [`ElectionFault::AlreadyServing`] when another process holds the
/// election, and [`ElectionFault::Storage`] when the state directory or the
/// election file cannot be prepared.
pub fn claim(root: &Path) -> Result<ElectionGuard, ElectionError> {
    let state_directory = root.join(RIFT_STATE_DIRECTORY);
    std::fs::create_dir_all(&state_directory).map_err(|error| {
        ElectionFault::storage("create state directory", &state_directory, error)
    })?;
    let election_path = state_directory.join(SERVER_ELECTION_FILE_NAME);
    let election_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&election_path)
        .map_err(|error| ElectionFault::storage("open election file", &election_path, error))?;
    match election_file.try_lock() {
        Ok(()) => Ok(ElectionGuard {
            election_file,
            document_path: state_directory.join(SERVER_LOCK_FILE_NAME),
            state_directory,
        }),
        Err(TryLockError::WouldBlock) => Err(Error::new(ElectionFault::AlreadyServing)),
        Err(TryLockError::Error(error)) => Err(ElectionFault::storage(
            "lock election file",
            &election_path,
            error,
        )),
    }
}

/// The held election: proof this process may publish and serve the
/// workspace.
///
/// The exclusive lock releases when the guard drops its file handle;
/// dropping also retires the published document, best effort.
#[derive(Debug)]
#[must_use = "dropping the guard releases the election"]
pub struct ElectionGuard {
    /// Held open for the guard's whole life; the advisory lock lives on it.
    election_file: std::fs::File,
    state_directory: PathBuf,
    document_path: PathBuf,
}

impl ElectionGuard {
    /// Publishes the lock document for readers, atomically.
    ///
    /// The document is validated, staged in a temp file inside `.rift`, and
    /// renamed over `server.json`, so a concurrent [`probe`] reads either
    /// the previous complete document or this one - never a partial write.
    /// Only the election holder can call this, which keeps `server.json`
    /// single-writer by construction.
    ///
    /// # Errors
    ///
    /// Returns [`ElectionFault::DocumentInvalid`] when the document breaks
    /// the [`ServerLock`] contract, and [`ElectionFault::Storage`] when
    /// staging or renaming fails.
    pub fn publish(&self, lock: &ServerLock) -> Result<(), ElectionError> {
        lock.validate()
            .map_err(|violation| Error::new(ElectionFault::DocumentInvalid(violation)))?;
        let bytes = serde_json::to_vec(lock).map_err(|error| {
            ElectionFault::storage(
                "serialize lock document",
                &self.document_path,
                io::Error::other(error),
            )
        })?;
        self.stage_and_rename(&bytes)
    }

    /// Writes `bytes` to a fresh temp file in `.rift` and renames it over
    /// the document path.
    fn stage_and_rename(&self, bytes: &[u8]) -> Result<(), ElectionError> {
        let stage_failed = |error: io::Error| {
            ElectionFault::storage("stage lock document", &self.state_directory, error)
        };
        let staged =
            tempfile::NamedTempFile::new_in(&self.state_directory).map_err(stage_failed)?;
        restrict_to_owner(staged.as_file()).map_err(stage_failed)?;
        staged.as_file().write_all(bytes).map_err(stage_failed)?;
        staged.as_file().sync_all().map_err(stage_failed)?;
        staged.persist(&self.document_path).map_err(|error| {
            ElectionFault::storage("publish lock document", &self.document_path, error.error)
        })?;
        Ok(())
    }

    /// Removes the published document, best effort.
    ///
    /// A document that cannot be removed is reported, not raised: once the
    /// election lock releases with this guard, [`probe`] classifies the
    /// leftover document as stale and the next starter replaces it.
    pub fn retire(&self) {
        match std::fs::remove_file(&self.document_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                component = "mcp",
                path = %self.document_path.display(),
                %error,
                "server lock document could not be removed"
            ),
        }
    }
}

impl Drop for ElectionGuard {
    fn drop(&mut self) {
        self.retire();
        // Closing the handle would release the advisory lock anyway; the
        // explicit unlock makes the release immediate on Windows, where a
        // close releases lazily.
        if let Err(error) = self.election_file.unlock() {
            tracing::debug!(component = "mcp", %error, "election lock release reported a failure");
        }
    }
}

/// What one workspace's lock state says about a serving process.
#[derive(Debug)]
pub enum ServerPresence {
    /// A live process holds the election and its published document
    /// validates: the port and token reach a serving `rift server`.
    Serving(ServerLock),
    /// Lock state exists but names no live server.
    Stale(StaleReason),
    /// No lock document exists for this workspace.
    Absent,
}

/// Why a workspace's lock state names no live server.
#[derive(Debug)]
pub enum StaleReason {
    /// `server.json` exists but could not be read.
    DocumentUnreadable,
    /// `server.json` is not a JSON document in the [`ServerLock`] shape.
    DocumentMalformed,
    /// `server.json` parsed but breaks the lock contract.
    DocumentInvalid(ServerLockViolation),
    /// The document validates but no process holds the election lock.
    ElectionUnheld,
    /// The election file exists but its lock state could not be observed.
    ElectionUnobservable,
}

/// Observes the workspace's lock state without blocking.
///
/// A [`ServerPresence::Serving`] answer requires both halves: the document
/// reads and validates, and a non-blocking shared lock on the election file
/// fails - proof a live process holds the exclusive lock. A shared lock
/// that succeeds is released immediately. The probe itself never waits and
/// never polls; callers that need to wait poll this function.
#[must_use]
pub fn probe(root: &Path) -> ServerPresence {
    match published_document(root) {
        Ok(lock) => classify_holder(root, lock),
        Err(presence) => presence,
    }
}

/// The published document when it exists, parses, and validates.
fn published_document(root: &Path) -> Result<ServerLock, ServerPresence> {
    let document_path = root.join(RIFT_STATE_DIRECTORY).join(SERVER_LOCK_FILE_NAME);
    let bytes = match std::fs::read(&document_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ServerPresence::Absent);
        }
        Err(_) => return Err(ServerPresence::Stale(StaleReason::DocumentUnreadable)),
    };
    let Ok(lock) = serde_json::from_slice::<ServerLock>(&bytes) else {
        return Err(ServerPresence::Stale(StaleReason::DocumentMalformed));
    };
    match lock.validate() {
        Ok(()) => Ok(lock),
        Err(violation) => Err(ServerPresence::Stale(StaleReason::DocumentInvalid(
            violation,
        ))),
    }
}

/// Restricts the staged document to its owner before the token is written.
///
/// The token separates OS users, so the file that carries it is never
/// readable by another user. Windows inherits the profile's ACLs instead;
/// the mode call has nothing to narrow there.
#[cfg(unix)]
fn restrict_to_owner(file: &std::fs::File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

/// See the unix arm: Windows scopes the file by the profile's ACLs.
#[cfg(not(unix))]
fn restrict_to_owner(_file: &std::fs::File) -> io::Result<()> {
    Ok(())
}

/// Whether a live process stands behind a validated document.
fn classify_holder(root: &Path, lock: ServerLock) -> ServerPresence {
    let election_path = root
        .join(RIFT_STATE_DIRECTORY)
        .join(SERVER_ELECTION_FILE_NAME);
    let election_file = match OpenOptions::new().read(true).open(&election_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ServerPresence::Stale(StaleReason::ElectionUnheld);
        }
        Err(_) => return ServerPresence::Stale(StaleReason::ElectionUnobservable),
    };
    match election_file.try_lock_shared() {
        // Nothing holds the exclusive lock; the probe's shared lock
        // releases with the handle at the end of this scope.
        Ok(()) => ServerPresence::Stale(StaleReason::ElectionUnheld),
        Err(TryLockError::WouldBlock) => ServerPresence::Serving(lock),
        Err(TryLockError::Error(_)) => ServerPresence::Stale(StaleReason::ElectionUnobservable),
    }
}

/// The serving server's published document, when one is live.
///
/// Convenience over [`probe`] for callers that only need the port and
/// token of a live server.
#[must_use]
pub fn read_serving(root: &Path) -> Option<ServerLock> {
    match probe(root) {
        ServerPresence::Serving(lock) => Some(lock),
        ServerPresence::Stale(_) | ServerPresence::Absent => None,
    }
}

/// The lock document this process publishes for one bound server.
fn served_document(port: u16, token: &str) -> ServerLock {
    ServerLock {
        port,
        token: token.to_owned(),
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    }
}

/// Serves the workspace at `root` as its elected server.
///
/// Claims the election, starts the HTTP transport, then publishes the lock
/// document, in that order, so a published document always names a bound
/// listener. When publishing fails the just-started server is shut down
/// again - bounded by `UNPUBLISHED_SHUTDOWN_DEADLINE` - before the error
/// returns, so no unreachable serving loop survives.
///
/// # Errors
///
/// Returns [`ElectionFault::AlreadyServing`] when another process holds the
/// election, and otherwise the claim, transport, or publish failure.
///
/// # Cancel safety
///
/// Dropping this future releases a claimed election. A transport already
/// started keeps its own serving tasks until `shutdown` cancels; the
/// election lock and any published document are gone, so probes classify
/// the leftover state as stale.
pub async fn serve_elected(
    root: &Path,
    shutdown: CancellationToken,
) -> Result<ElectedServer, ElectionError> {
    let guard = claim(root)?;
    let serving_stop = shutdown.child_token();
    let server = serve_http(root, serving_stop.clone())
        .await
        .map_err(ElectionFault::serve)?;
    let document = served_document(server.port(), server.token());
    match guard.publish(&document) {
        Ok(()) => Ok(ElectedServer { server, guard }),
        Err(error) => Err(shut_down_unpublished(server, &serving_stop, error).await),
    }
}

/// Stops a server whose document never published and returns the publish
/// failure.
///
/// The shutdown wait is bounded by [`UNPUBLISHED_SHUTDOWN_DEADLINE`]; a
/// server that misses it is reported and abandoned to its cancelled token.
async fn shut_down_unpublished(
    server: HttpServer,
    serving_stop: &CancellationToken,
    publish_failure: ElectionError,
) -> ElectionError {
    serving_stop.cancel();
    match tokio::time::timeout(UNPUBLISHED_SHUTDOWN_DEADLINE, server.stopped()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            component = "mcp",
            %error,
            "unpublished server reported a shutdown failure"
        ),
        Err(_elapsed) => tracing::warn!(
            component = "mcp",
            deadline = ?UNPUBLISHED_SHUTDOWN_DEADLINE,
            "unpublished server missed its shutdown deadline"
        ),
    }
    publish_failure
}

/// One elected serving server: the HTTP transport plus the held election.
#[derive(Debug)]
#[must_use = "an elected server is driven through `stopped`"]
pub struct ElectedServer {
    server: HttpServer,
    guard: ElectionGuard,
}

impl ElectedServer {
    /// The loopback port the elected server accepts requests on.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.server.port()
    }

    /// Waits until the server stopped, then retires the lock document.
    ///
    /// # Errors
    ///
    /// Returns the transport's shutdown failure; the document is retired
    /// and the election released on every path.
    ///
    /// # Cancel safety
    ///
    /// Dropping this future retires the document and releases the election
    /// through the guard's drop; the serving tasks detach and complete a
    /// shutdown already triggered in the background.
    pub async fn stopped(self) -> Result<(), ElectionError> {
        let outcome = self.server.stopped().await.map_err(ElectionFault::serve);
        self.guard.retire();
        outcome
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use rift_protocol::lock::{
        SERVER_LOCK_FILE_NAME, SERVER_PORT_MIN, SERVER_TOKEN_LENGTH, ServerLock,
    };

    use super::{
        ElectionFault, SERVER_ELECTION_FILE_NAME, ServerPresence, StaleReason, claim, probe,
        read_serving, served_document,
    };

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    /// Publishes per round in the atomicity test.
    const PUBLISH_ROUND_COUNT: usize = 200;
    /// Caps the concurrent reader's observation loop.
    const READ_ATTEMPT_COUNT_MAX: usize = 100_000;

    fn valid_document() -> ServerLock {
        ServerLock {
            port: SERVER_PORT_MIN,
            token: "a".repeat(SERVER_TOKEN_LENGTH),
            pid: 4_242,
            version: "0.0.11".to_owned(),
        }
    }

    fn document_path(root: &std::path::Path) -> std::path::PathBuf {
        root.join(".rift").join(SERVER_LOCK_FILE_NAME)
    }

    #[test]
    fn claim_creates_the_state_directory_and_election_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = claim(directory.path())?;
        let election_path = directory
            .path()
            .join(".rift")
            .join(SERVER_ELECTION_FILE_NAME);
        assert!(election_path.is_file(), "election file must exist");
        assert_eq!(
            fs::metadata(&election_path)?.len(),
            0,
            "the election file carries no content"
        );
        Ok(())
    }

    // The std lock is `flock` (unix) and `LockFileEx` (windows), both of
    // which attach to the open file description: a second open of the same
    // path conflicts even inside one process, so the refusal is provable
    // without spawning.
    #[test]
    fn second_claim_is_refused_while_the_first_holds() -> TestResult {
        let directory = tempfile::tempdir()?;
        let first = claim(directory.path())?;
        let refused = claim(directory.path()).expect_err("held election must refuse");
        assert!(
            matches!(refused.fault(), ElectionFault::AlreadyServing),
            "refusal must be the typed AlreadyServing outcome: {refused:?}"
        );
        assert_eq!(refused.descriptor().code(), "server_already_serving");
        drop(first);
        let _reclaimed = claim(directory.path())?;
        Ok(())
    }

    #[test]
    fn publish_writes_a_validating_document_and_retire_removes_it() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        let document = valid_document();
        guard.publish(&document)?;
        let bytes = fs::read(document_path(directory.path()))?;
        let published: ServerLock = serde_json::from_slice(&bytes)?;
        assert_eq!(published, document);
        guard.retire();
        assert!(
            !document_path(directory.path()).exists(),
            "retire must remove the document"
        );
        guard.retire();
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn published_document_is_readable_by_its_owner_alone() -> TestResult {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        guard.publish(&valid_document())?;
        let mode = fs::metadata(document_path(directory.path()))?
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the token-carrying document must be owner-only, got mode {mode:o}"
        );
        Ok(())
    }

    #[test]
    fn dropping_the_guard_retires_the_document() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        guard.publish(&valid_document())?;
        drop(guard);
        assert!(
            !document_path(directory.path()).exists(),
            "drop must retire the document"
        );
        Ok(())
    }

    #[test]
    fn publish_refuses_a_document_breaking_the_contract() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        let mut document = valid_document();
        document.pid = 0;
        let error = guard
            .publish(&document)
            .expect_err("invalid document must refuse");
        assert!(matches!(error.fault(), ElectionFault::DocumentInvalid(_)));
        assert_eq!(error.descriptor().code(), "internal_error");
        assert!(
            !document_path(directory.path()).exists(),
            "a refused publish must write nothing"
        );
        Ok(())
    }

    #[test]
    fn probe_reports_absent_without_lock_state() -> TestResult {
        let directory = tempfile::tempdir()?;
        assert!(matches!(probe(directory.path()), ServerPresence::Absent));
        assert!(read_serving(directory.path()).is_none());
        Ok(())
    }

    #[test]
    fn probe_classifies_defective_documents_as_stale() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = document_path(directory.path());
        fs::create_dir_all(path.parent().ok_or("document path must have a parent")?)?;

        fs::write(&path, b"not json")?;
        assert!(matches!(
            probe(directory.path()),
            ServerPresence::Stale(StaleReason::DocumentMalformed)
        ));

        let mut invalid = valid_document();
        invalid.pid = 0;
        fs::write(&path, serde_json::to_vec(&invalid)?)?;
        assert!(matches!(
            probe(directory.path()),
            ServerPresence::Stale(StaleReason::DocumentInvalid(_))
        ));
        assert!(read_serving(directory.path()).is_none());
        Ok(())
    }

    #[test]
    fn probe_classifies_a_valid_document_without_a_holder_as_stale() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = document_path(directory.path());
        fs::create_dir_all(path.parent().ok_or("document path must have a parent")?)?;
        fs::write(&path, serde_json::to_vec(&valid_document())?)?;

        // No election file at all: nobody ever claimed.
        assert!(matches!(
            probe(directory.path()),
            ServerPresence::Stale(StaleReason::ElectionUnheld)
        ));

        // An election file nobody locks: the claimer is gone.
        fs::write(
            directory
                .path()
                .join(".rift")
                .join(SERVER_ELECTION_FILE_NAME),
            b"",
        )?;
        assert!(matches!(
            probe(directory.path()),
            ServerPresence::Stale(StaleReason::ElectionUnheld)
        ));
        Ok(())
    }

    #[test]
    fn probe_reports_a_held_election_with_a_valid_document_as_serving() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        let document = valid_document();
        guard.publish(&document)?;
        match probe(directory.path()) {
            ServerPresence::Serving(lock) => assert_eq!(lock, document),
            other => panic!("held election with a valid document must serve: {other:?}"),
        }
        let read = read_serving(directory.path()).ok_or("read_serving must see the server")?;
        assert_eq!(read, document);
        Ok(())
    }

    #[test]
    fn concurrent_readers_never_observe_a_partial_document() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        guard.publish(&valid_document())?;
        let path = document_path(directory.path());
        let writer_done = Arc::new(AtomicBool::new(false));
        let reader_stop = Arc::clone(&writer_done);
        let reader = std::thread::spawn(move || {
            let mut observed = 0_usize;
            for _ in 0..READ_ATTEMPT_COUNT_MAX {
                if reader_stop.load(Ordering::Acquire) {
                    break;
                }
                let bytes = fs::read(&path).expect("the published document must always exist");
                let lock: ServerLock = serde_json::from_slice(&bytes)
                    .expect("a reader must never observe a partial document");
                lock.validate()
                    .expect("every observed document must validate");
                observed += 1;
            }
            observed
        });
        for _ in 0..PUBLISH_ROUND_COUNT {
            guard.publish(&valid_document())?;
        }
        writer_done.store(true, Ordering::Release);
        let observed = reader.join().map_err(|_panic| "reader panicked")?;
        assert!(observed > 0, "the reader must have observed documents");
        Ok(())
    }

    #[test]
    fn served_document_records_this_process_and_release() {
        let token = "a".repeat(SERVER_TOKEN_LENGTH);
        let document = served_document(SERVER_PORT_MIN, &token);
        assert_eq!(document.pid, std::process::id());
        assert_eq!(document.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(document.port, SERVER_PORT_MIN);
        assert_eq!(document.token, token);
        assert_eq!(document.validate(), Ok(()));
    }

    #[test]
    fn election_faults_carry_registry_codes_and_evidence() {
        let storage = ElectionFault::storage(
            "open election file",
            std::path::Path::new("/workspace/.rift/server.lock"),
            std::io::Error::other("disk gone"),
        );
        assert_eq!(storage.descriptor().code(), "storage_failure");
        let rendered = storage.to_string();
        assert!(rendered.contains("open election file"), "{rendered}");
        assert!(rendered.contains("server.lock"), "{rendered}");
        assert!(
            std::error::Error::source(&storage).is_some(),
            "storage faults keep their io source"
        );
    }
}
