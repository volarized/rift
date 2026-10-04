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
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rift_core::constants::RIFT_STATE_DIRECTORY;
use rift_error::{ErrorContext, RiftError, causes, errors};
use rift_index::WorkspaceIndexLimits;
use rift_protocol::configuration::ServerConfiguration;
use rift_protocol::lock::{SERVER_LOCK_FILE_NAME, ServerLock, ServerLockViolation};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use crate::http::serve_http;
use crate::http::{
    DeferredDatabaseShutdown, HttpServer, TokenCheck, serve_http_with_storage,
    serve_repository_http,
};
use crate::identity::BuildCheckout;
use crate::repository::repository_election_directory;
use crate::storage::WorkspaceStorage;

/// The zero-byte election file's name under the `.rift` state directory.
///
/// The protocol crate owns only `server.json`'s name; the election file is
/// this module's implementation detail.
const SERVER_ELECTION_FILE_NAME: &str = "server.lock";

/// Longest wait for a server that failed to publish to shut down again.
const UNPUBLISHED_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(10);

/// Bound on one probe's connect to the recorded port.
///
/// The port is a loopback one, where a connect is accepted or refused at
/// once; the bound only keeps a probe from hanging on a filtered socket.
const PRESENCE_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

fn election_document_invalid(violation: ServerLockViolation) -> RiftError {
    let mut builder = errors::mcp::election_document_invalid();
    for (key, value) in violation.evidence() {
        builder = builder.with(ErrorContext::new(key, value));
    }
    builder.error()
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
/// Returns a registered error when another process holds the election or
/// the state directory or election file cannot be prepared.
pub fn claim(root: &Path) -> Result<ElectionGuard, RiftError> {
    claim_state_directory(&root.join(RIFT_STATE_DIRECTORY))
}

/// Claims an election whose files live in `state_directory`.
pub(crate) fn claim_state_directory(state_directory: &Path) -> Result<ElectionGuard, RiftError> {
    let state_directory = state_directory.to_path_buf();
    std::fs::create_dir_all(&state_directory).map_err(|source| {
        errors::mcp::election_storage_failed()
            .operation("create state directory")
            .path(&state_directory)
            .source(source)
            .error()
    })?;
    let election_path = state_directory.join(SERVER_ELECTION_FILE_NAME);
    let election_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&election_path)
        .map_err(|source| {
            errors::mcp::election_storage_failed()
                .operation("open election file")
                .path(&election_path)
                .source(source)
                .error()
        })?;
    match election_file.try_lock() {
        Ok(()) => {
            let guard = ElectionGuard {
                election_file,
                document_path: document_path_in(&state_directory),
                state_directory,
            };
            // The free election proves any leftover document stale. Scrub it
            // now, before serving starts, so a concurrent probe cannot pair
            // the previous holder's document with this holder's lock.
            guard.retire();
            Ok(guard)
        }
        Err(TryLockError::WouldBlock) => errors::mcp::election_already_serving().fail(),
        Err(TryLockError::Error(source)) => errors::mcp::election_storage_failed()
            .operation("lock election file")
            .path(&election_path)
            .source(source)
            .fail(),
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
    /// Checks that this guard holds the requested workspace's election.
    pub(crate) fn validate_workspace(&self, root: &Path) -> Result<(), RiftError> {
        let state = root.join(RIFT_STATE_DIRECTORY);
        let requested = std::fs::canonicalize(&state).map_err(|source| {
            errors::mcp::election_storage_failed()
                .operation("read election directory")
                .path(&state)
                .source(source)
                .error()
        })?;
        let held = std::fs::canonicalize(&self.state_directory).map_err(|source| {
            errors::mcp::election_storage_failed()
                .operation("read election directory")
                .path(&self.state_directory)
                .source(source)
                .error()
        })?;
        if requested != held {
            return errors::mcp::election_storage_failed()
                .operation("validate workspace election")
                .path(root)
                .source(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "election belongs to another workspace",
                ))
                .fail();
        }
        Ok(())
    }

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
    /// Returns a registered error when the document breaks the [`ServerLock`]
    /// contract or staging or renaming fails.
    pub fn publish(&self, lock: &ServerLock) -> Result<(), RiftError> {
        lock.validate().map_err(election_document_invalid)?;
        let bytes = serde_json::to_vec(lock).map_err(|error| {
            errors::mcp::election_storage_failed()
                .operation("serialize lock document")
                .path(&self.document_path)
                .source(io::Error::other(error))
                .error()
        })?;
        self.stage_and_rename(&bytes)
    }

    /// Writes `bytes` to a fresh temp file in `.rift` and renames it over
    /// the document path.
    ///
    /// The rename is `std::fs::rename`, not `NamedTempFile::persist`: on
    /// Windows `persist` calls `MoveFileExW` alone, which refuses with
    /// `Access is denied` to replace a document a concurrent [`probe`] holds
    /// open, while `std::fs::rename` retries that refusal as a POSIX-semantics
    /// rename, which replaces it. A staged file the rename leaves behind is
    /// removed best effort.
    fn stage_and_rename(&self, bytes: &[u8]) -> Result<(), RiftError> {
        let staged = tempfile::NamedTempFile::new_in(&self.state_directory).map_err(|source| {
            errors::mcp::election_storage_failed()
                .operation("stage lock document")
                .path(&self.state_directory)
                .source(source)
                .error()
        })?;
        restrict_to_owner(staged.as_file()).map_err(|source| {
            errors::mcp::election_storage_failed()
                .operation("stage lock document")
                .path(&self.state_directory)
                .source(source)
                .error()
        })?;
        staged.as_file().write_all(bytes).map_err(|source| {
            errors::mcp::election_storage_failed()
                .operation("stage lock document")
                .path(&self.state_directory)
                .source(source)
                .error()
        })?;
        staged.as_file().sync_all().map_err(|source| {
            errors::mcp::election_storage_failed()
                .operation("stage lock document")
                .path(&self.state_directory)
                .source(source)
                .error()
        })?;
        let staged = staged.into_temp_path().keep().map_err(|error| {
            errors::mcp::election_storage_failed()
                .operation("stage lock document")
                .path(&self.state_directory)
                .source(error.error)
                .error()
        })?;
        std::fs::rename(&staged, &self.document_path).map_err(|error| {
            let _ = std::fs::remove_file(&staged);
            errors::mcp::election_storage_failed()
                .operation("publish lock document")
                .path(&self.document_path)
                .source(error)
                .error()
        })?;
        Ok(())
    }

    /// Removes the published document, or empties it when it cannot be removed.
    ///
    /// A removal can fail where a truncation succeeds: unlinking needs the
    /// state directory writable and, on a full volume, room for the
    /// directory's own update. An emptied document is malformed, so no
    /// [`probe`] pairs the previous holder's facts with this holder's lock.
    /// A document that cannot be emptied either is reported, not raised:
    /// once the election lock releases with this guard, [`probe`] classifies
    /// the leftover as stale and the next starter replaces it.
    pub fn retire(&self) {
        match std::fs::remove_file(&self.document_path) {
            Ok(()) => return,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => tracing::warn!(
                component = "mcp",
                path = %self.document_path.display(),
                %error,
                "server lock document could not be removed; emptying it instead"
            ),
        }
        if let Err(error) = std::fs::File::create(&self.document_path) {
            tracing::warn!(
                component = "mcp",
                path = %self.document_path.display(),
                %error,
                "server lock document could not be emptied"
            );
        }
    }
}

impl Drop for ElectionGuard {
    fn drop(&mut self) {
        self.retire();
        release_election_lock(&self.election_file);
    }
}

/// Releases this process's lock on the election file ahead of closing it.
///
/// Closing the handle releases the lock too, but Windows releases a closed
/// handle's locks lazily: `LockFileEx` states that "the time it takes for the
/// operating system to unlock these locks depends upon available system
/// resources". A claim that meets such a lock loses the election although no
/// process holds it, so every lock this module takes is released explicitly.
fn release_election_lock(election_file: &std::fs::File) {
    if let Err(error) = election_file.unlock() {
        tracing::debug!(component = "mcp", %error, "election lock release reported a failure");
    }
}

/// What one workspace's lock state says about a serving process.
#[derive(Debug)]
pub enum ServerPresence {
    /// A live process holds the election, its published document
    /// validates, and the recorded port accepts a connect: the port and
    /// token reach a serving `rift server`.
    Serving(ServerLock),
    /// A live process holds the election but has published no document
    /// yet: the elected server is still building its first index.
    Starting,
    /// Lock state exists but names no live server.
    Stale(StaleReason),
    /// No lock document exists for this workspace.
    Absent,
}

impl ServerPresence {
    /// Whether a live process holds the election, whatever it has published.
    ///
    /// A starter that spawns under a held election only loses it; a stop
    /// is complete only once the election releases.
    #[must_use]
    pub fn election_held(&self) -> bool {
        matches!(
            self,
            Self::Serving(_) | Self::Starting | Self::Stale(StaleReason::PortUnreachable { .. })
        )
    }
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
    /// A live process holds the election and its document validates, but
    /// the recorded port refuses a connect: the holder, whose pid the
    /// document recorded, is shutting down.
    PortUnreachable {
        /// The pid the document recorded.
        pid: u32,
    },
}

/// What the election file says about a holder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElectionState {
    /// A live process holds the exclusive lock.
    Held,
    /// Nothing holds the lock, or no election file exists.
    Unheld,
    /// The election file exists but its lock state could not be observed.
    Unobservable,
}

/// Observes the workspace's lock state without blocking.
///
/// A [`ServerPresence::Serving`] answer requires every half: a non-blocking
/// shared lock on the election file fails - proof a live process holds the
/// exclusive lock - the document reads and validates, and the recorded port
/// accepts a connect within `PRESENCE_CONNECT_TIMEOUT`. A held election
/// with no validating document is a server still starting; a held election
/// whose recorded port refuses is a server shutting down. A shared lock that
/// succeeds is released immediately. The probe itself never waits and never
/// polls; callers that need to wait poll this function.
#[must_use]
pub fn probe(root: &Path) -> ServerPresence {
    probe_state_directory(&root.join(RIFT_STATE_DIRECTORY))
}

/// Observes an election whose files live in `state_directory`.
#[must_use]
#[doc(hidden)]
pub fn probe_state_directory(state_directory: &Path) -> ServerPresence {
    match (
        election_state_in(state_directory),
        published_document_in(state_directory),
    ) {
        (ElectionState::Held, Ok(lock)) if port_answers(lock.port) => ServerPresence::Serving(lock),
        (ElectionState::Held, Ok(lock)) => {
            ServerPresence::Stale(StaleReason::PortUnreachable { pid: lock.pid })
        }
        (ElectionState::Held, Err(_)) => ServerPresence::Starting,
        (ElectionState::Unheld, Ok(_)) => ServerPresence::Stale(StaleReason::ElectionUnheld),
        (ElectionState::Unobservable, Ok(_)) => {
            ServerPresence::Stale(StaleReason::ElectionUnobservable)
        }
        (_, Err(presence)) => *presence,
    }
}

/// Whether the recorded loopback port accepts a connect right now.
fn port_answers(port: u16) -> bool {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&address, PRESENCE_CONNECT_TIMEOUT).is_ok()
}

/// The workspace's lock document path, `.rift/server.json` below `root`.
#[must_use]
pub fn document_path(root: &Path) -> PathBuf {
    document_path_in(&root.join(RIFT_STATE_DIRECTORY))
}

/// The lock document in an explicit `.rift` state directory.
#[must_use]
pub(crate) fn document_path_in(state_directory: &Path) -> PathBuf {
    state_directory.join(SERVER_LOCK_FILE_NAME)
}

/// The published document when it exists, parses, and validates.
fn published_document_in(state_directory: &Path) -> Result<ServerLock, Box<ServerPresence>> {
    let bytes = match std::fs::read(document_path_in(state_directory)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(Box::new(ServerPresence::Absent));
        }
        Err(_) => {
            return Err(Box::new(ServerPresence::Stale(
                StaleReason::DocumentUnreadable,
            )));
        }
    };
    let Ok(lock) = serde_json::from_slice::<ServerLock>(&bytes) else {
        return Err(Box::new(ServerPresence::Stale(
            StaleReason::DocumentMalformed,
        )));
    };
    match lock.validate() {
        Ok(()) => Ok(lock),
        Err(violation) => Err(Box::new(ServerPresence::Stale(
            StaleReason::DocumentInvalid(violation),
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

fn election_state_in(state_directory: &Path) -> ElectionState {
    let election_path = state_directory.join(SERVER_ELECTION_FILE_NAME);
    let election_file = match OpenOptions::new().read(true).open(&election_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return ElectionState::Unheld,
        Err(_) => return ElectionState::Unobservable,
    };
    match election_file.try_lock_shared() {
        // Nothing holds the exclusive lock. The probe releases its shared
        // lock at once, ahead of the handle's close.
        Ok(()) => {
            release_election_lock(&election_file);
            ElectionState::Unheld
        }
        Err(TryLockError::WouldBlock) => ElectionState::Held,
        Err(TryLockError::Error(_)) => ElectionState::Unobservable,
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
        ServerPresence::Starting | ServerPresence::Stale(_) | ServerPresence::Absent => None,
    }
}

/// The lock document this process publishes for one bound server.
fn served_document(
    port: u16,
    token: &str,
    identity: rift_protocol::lock::ProductIdentity,
    server: Option<rift_protocol::configuration::ServerConfiguration>,
) -> ServerLock {
    ServerLock {
        port,
        token: token.to_owned(),
        pid: std::process::id(),
        identity,
        server,
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
/// Requests are served under [`TokenCheck::Required`], and the server names
/// itself as [`BuildCheckout::Unversioned`], as [`crate::RiftMcp::build`] does.
///
/// # Errors
///
/// Returns a registered error when another process holds the election or
/// the claim, transport, or publish operation fails.
///
/// # Cancel safety
///
/// The SQLite worker retains the election until its actual exit, including after
/// cancellation or a shutdown timeout. A transport already started keeps its serving
/// tasks until `shutdown` cancels.
pub async fn serve_elected(
    root: &Path,
    shutdown: CancellationToken,
) -> Result<ElectedServer, RiftError> {
    let guard = Arc::new(claim(root)?);
    let storage = WorkspaceStorage::open_elected(root, Arc::clone(&guard)).await?;
    serve_elected_with_storage(
        root,
        guard,
        shutdown,
        storage,
        TokenCheck::Required,
        BuildCheckout::Unversioned,
    )
    .await
}

/// Serves the workspace through storage opened under its held election.
///
/// The caller claims `root` before calling [`WorkspaceStorage::open_elected`], then
/// passes the same guard here. A guard for another workspace is refused before serving.
/// Requests follow `check`, and the server names the build `checkout` describes.
///
/// # Errors
///
/// Returns the same failures as [`serve_elected`].
///
/// # Cancel safety
///
/// Dropping this future follows [`serve_elected`]'s cancellation behavior.
pub async fn serve_elected_with_storage(
    root: &Path,
    guard: Arc<ElectionGuard>,
    shutdown: CancellationToken,
    storage: WorkspaceStorage,
    check: TokenCheck,
    checkout: BuildCheckout,
) -> Result<ElectedServer, RiftError> {
    serve_elected_at(
        root,
        guard,
        shutdown,
        storage,
        WorkspaceIndexLimits::default(),
        check,
        checkout,
    )
    .await
}

/// Serves repository workspaces under one repository election.
#[doc(hidden)]
pub async fn serve_repository_elected(
    authority_root: &Path,
    common_directory: &Path,
    server_configuration: ServerConfiguration,
    shutdown: CancellationToken,
    limits: WorkspaceIndexLimits,
    check: TokenCheck,
    checkout: BuildCheckout,
) -> Result<ElectedServer, RiftError> {
    let identity = crate::identity::product_identity(checkout)
        .await
        .map_err(|source| {
            errors::mcp::election_storage_failed()
                .operation("read product identity")
                .path(common_directory)
                .source(source)
                .error()
        })?;
    let state_directory =
        repository_election_directory(common_directory, &identity).map_err(|error| {
            errors::mcp::election_storage_failed()
                .operation("select repository election")
                .path(common_directory)
                .source(io::Error::other(error))
                .error()
        })?;
    let guard = Arc::new(claim_state_directory(&state_directory)?);
    let serving_stop = shutdown.child_token();
    let server = serve_repository_http(
        authority_root,
        common_directory,
        server_configuration,
        serving_stop.clone(),
        limits,
        check,
        checkout,
    )
    .await?;
    let document = served_document(
        server.port(),
        server.token(),
        server.product_identity().clone(),
        Some(server.server_configuration().clone()),
    );
    match guard.publish(&document) {
        Ok(()) => Ok(ElectedServer { server, guard }),
        Err(error) => shut_down_unpublished(server, &serving_stop, error)
            .await
            .fail(),
    }
}

/// Serves the workspace under explicit index bounds and one token policy,
/// recording a start that fails before it returns.
///
/// The caller holds the election before storage opens. Build, transport, and
/// publication failures are recorded through the workspace's log drain. An election
/// refusal occurs before that drain starts and returns to the caller for stderr.
///
/// # Errors
///
/// Returns the same failures as [`serve_elected`].
///
/// # Cancel safety
///
/// Dropping this future follows [`serve_elected`]'s cancellation behavior.
pub(crate) async fn serve_elected_at(
    root: &Path,
    guard: Arc<ElectionGuard>,
    shutdown: CancellationToken,
    storage: WorkspaceStorage,
    limits: WorkspaceIndexLimits,
    check: TokenCheck,
    checkout: BuildCheckout,
) -> Result<ElectedServer, RiftError> {
    let elected = elect_and_serve(root, guard, shutdown, storage, limits, check, checkout).await;
    if let Err(error) = &elected {
        record_start_failure(error);
    }
    elected
}

/// Records why this process will not serve, before the caller exits on it.
fn record_start_failure(error: &RiftError) {
    if error.slug() == errors::mcp::election_already_serving::SLUG {
        tracing::info!(
            component = "mcp",
            operation = "server.start",
            "another rift server already serves this workspace; this process exits"
        );
        return;
    }
    let causes = causes(error).join(": ");
    tracing::error!(
        component = "mcp",
        operation = "server.start",
        error = %error,
        causes,
        "the server failed to start and exits"
    );
}

/// Validates the held election, builds, binds, and publishes, in that order.
async fn elect_and_serve(
    root: &Path,
    guard: Arc<ElectionGuard>,
    shutdown: CancellationToken,
    storage: WorkspaceStorage,
    limits: WorkspaceIndexLimits,
    check: TokenCheck,
    checkout: BuildCheckout,
) -> Result<ElectedServer, RiftError> {
    guard.validate_workspace(root)?;
    if !storage.holds_election(&guard) {
        return errors::mcp::election_storage_failed()
            .operation("validate workspace storage")
            .path(root)
            .source(io::Error::new(
                io::ErrorKind::InvalidInput,
                "storage opened without this workspace election",
            ))
            .fail();
    }
    let serving_stop = shutdown.child_token();
    let server =
        serve_http_with_storage(root, serving_stop.clone(), storage, limits, check, checkout)
            .await?;
    let document = served_document(
        server.port(),
        server.token(),
        server.product_identity().clone(),
        Some(server.server_configuration().clone()),
    );
    match guard.publish(&document) {
        Ok(()) => Ok(ElectedServer { server, guard }),
        Err(error) => shut_down_unpublished(server, &serving_stop, error)
            .await
            .fail(),
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
    publish_failure: RiftError,
) -> RiftError {
    serving_stop.cancel();
    let (_deadline, outcome) = server.stopped(UNPUBLISHED_SHUTDOWN_DEADLINE).await;
    if let Err(error) = outcome {
        tracing::warn!(
            component = "mcp",
            %error,
            "unpublished server reported a shutdown failure"
        );
    }
    publish_failure
}

/// One elected serving server: the HTTP transport plus the held election.
#[derive(Debug)]
#[must_use = "an elected server is driven through `stopped`"]
pub struct ElectedServer {
    server: HttpServer,
    guard: Arc<ElectionGuard>,
}

impl ElectedServer {
    /// The loopback port the elected server accepts requests on.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.server.port()
    }

    /// Waits until the server stopped, bounded by `budget` from the moment
    /// the stop began, and hands the election guard back so the caller
    /// releases it last.
    ///
    /// The document is not retired here: the caller drops the returned guard
    /// after every later stop stage, the log drain's final flush included, so
    /// the election releases immediately before the process leaves. The
    /// returned deadline is the stop's shared one, so those later stages take
    /// only what the transport shutdown left of `budget`.
    ///
    /// # Errors
    ///
    /// The third tuple element carries the transport's shutdown failure.
    ///
    /// # Cancel safety
    ///
    /// Dropping this future leaves the election held by a live SQLite worker.
    /// Serving tasks complete a shutdown already triggered in the background.
    pub async fn stopped(
        self,
        budget: Duration,
    ) -> (Arc<ElectionGuard>, Instant, Result<(), RiftError>) {
        let (guard, deadline, stopped, database) = self.stopped_before_database(budget).await;
        let database = database.shutdown(deadline).await;
        (guard, deadline, stopped.and(database))
    }

    /// Stops serving before SQLite close, for a caller with final store writes.
    #[doc(hidden)]
    pub async fn stopped_before_database(
        self,
        budget: Duration,
    ) -> (
        Arc<ElectionGuard>,
        Instant,
        Result<(), RiftError>,
        DeferredDatabaseShutdown,
    ) {
        let (deadline, outcome, database) = self.server.stopped_before_database(budget).await;
        (self.guard, deadline, outcome, database)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use rift_error::errors;
    use rift_protocol::lock::{
        SERVER_LOCK_FILE_NAME, SERVER_PORT_MIN, SERVER_TOKEN_LENGTH, ServerLock,
    };
    use tokio_util::sync::CancellationToken;

    use crate::http::TokenCheck;

    use super::{
        SERVER_ELECTION_FILE_NAME, ServerPresence, StaleReason, claim, probe, read_serving,
        serve_elected, serve_elected_at, serve_http, served_document, shut_down_unpublished,
    };

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    /// A loopback listener a probe's connect reaches, and the port it holds.
    fn answering_port() -> TestResult<(std::net::TcpListener, u16)> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        Ok((listener, port))
    }

    /// A loopback port nothing listens on: bound to learn the number, then released.
    fn dead_port() -> TestResult<u16> {
        let (listener, port) = answering_port()?;
        drop(listener);
        Ok(port)
    }

    /// Publishes per round in the atomicity test.
    const PUBLISH_ROUND_COUNT: usize = 200;
    /// Caps the concurrent reader's observation loop.
    const READ_ATTEMPT_COUNT_MAX: usize = 100_000;

    fn valid_document() -> ServerLock {
        ServerLock {
            port: SERVER_PORT_MIN,
            token: "a".repeat(SERVER_TOKEN_LENGTH),
            pid: 4_242,
            identity: rift_protocol::lock::ProductIdentity {
                version: "0.0.11".to_owned(),
                schema_digest: "b".repeat(64),
            },
            server: None,
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
        assert_eq!(refused.slug(), errors::mcp::election_already_serving::SLUG);
        drop(first);
        let _reclaimed = claim(directory.path())?;
        Ok(())
    }

    /// A free election proves any leftover document stale, so the claim
    /// scrubs it before serving starts and a probe in the claim-to-publish
    /// window reads absence, never the previous holder's facts.
    #[test]
    fn claim_scrubs_the_previous_holder_document() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir_all(directory.path().join(".rift"))?;
        fs::write(
            document_path(directory.path()),
            serde_json::to_vec(&valid_document())?,
        )?;
        let _guard = claim(directory.path())?;
        assert!(
            !document_path(directory.path()).exists(),
            "the claim must scrub the leftover document"
        );
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
        assert_eq!(error.slug(), errors::mcp::election_document_invalid::SLUG);
        assert!(error.context().any(|(key, _)| key == "pid"));
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
    fn probe_reports_a_held_election_with_an_answering_document_as_serving() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        let (_listener, port) = answering_port()?;
        let document = ServerLock {
            port,
            ..valid_document()
        };
        guard.publish(&document)?;
        match probe(directory.path()) {
            ServerPresence::Serving(lock) => assert_eq!(lock, document),
            other => panic!("held election with an answering document must serve: {other:?}"),
        }
        let read = read_serving(directory.path()).ok_or("read_serving must see the server")?;
        assert_eq!(read, document);
        assert!(probe(directory.path()).election_held());
        Ok(())
    }

    /// The document alone is not an answer: a holder whose listener is gone is shutting
    /// down, and the recorded port says so before the election releases.
    #[test]
    fn probe_reports_a_held_election_whose_port_refuses_as_stale() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        let document = ServerLock {
            port: dead_port()?,
            ..valid_document()
        };
        guard.publish(&document)?;
        let presence = probe(directory.path());
        assert!(
            matches!(
                presence,
                ServerPresence::Stale(StaleReason::PortUnreachable { pid: 4_242 })
            ),
            "{presence:?}"
        );
        assert!(
            presence.election_held(),
            "the holder is still alive: {presence:?}"
        );
        assert!(read_serving(directory.path()).is_none());
        Ok(())
    }

    #[test]
    fn probe_reports_a_held_election_without_a_document_as_starting() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = claim(directory.path())?;
        let presence = probe(directory.path());
        assert!(matches!(presence, ServerPresence::Starting), "{presence:?}");
        assert!(presence.election_held());
        assert!(read_serving(directory.path()).is_none());
        assert!(
            !ServerPresence::Absent.election_held()
                && !ServerPresence::Stale(StaleReason::ElectionUnheld).election_held()
        );
        Ok(())
    }

    /// A claim whose scrub cannot unlink the previous holder's document empties it
    /// instead, so the probe reads a starting server, never the previous holder's port
    /// under this holder's lock.
    #[cfg(unix)]
    #[test]
    fn claim_empties_a_previous_document_it_cannot_remove() -> TestResult {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let state_directory = directory.path().join(".rift");
        fs::create_dir_all(&state_directory)?;
        fs::write(state_directory.join(SERVER_ELECTION_FILE_NAME), b"")?;
        let (_listener, port) = answering_port()?;
        let previous = ServerLock {
            port,
            ..valid_document()
        };
        let previous_bytes = serde_json::to_vec(&previous)?;
        fs::write(document_path(directory.path()), previous_bytes)?;
        let saved = fs::metadata(&state_directory)?.permissions();
        fs::set_permissions(&state_directory, fs::Permissions::from_mode(0o500))?;

        let claimed = claim(directory.path());
        let presence = probe(directory.path());
        let leftover = fs::read(document_path(directory.path()));
        fs::set_permissions(&state_directory, saved)?;

        let _guard = claimed?;
        assert!(
            matches!(presence, ServerPresence::Starting),
            "the previous holder's document must not pair with this lock: {presence:?}"
        );
        assert!(
            leftover?.is_empty(),
            "the document the scrub could not remove is emptied"
        );
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

    /// The elected server starts before its bounded workspace index finishes.
    #[tokio::test]
    async fn a_workspace_over_files_max_does_not_block_elected_server_start() -> TestResult {
        const SOURCE_FILES: usize = 1_001;
        const SOURCE_FILES_MAX: usize = 1_000;
        let directory = tempfile::tempdir()?;
        for file in 0..SOURCE_FILES {
            fs::write(directory.path().join(format!("unit_{file:04}.rs")), "")?;
        }
        crate::server::hermetic_workspace(
            directory.path(),
            &format!("[source]\nfiles = {SOURCE_FILES_MAX}\n"),
        )?;
        let guard = Arc::new(claim(directory.path())?);
        let storage =
            crate::storage::WorkspaceStorage::open_elected(directory.path(), Arc::clone(&guard))
                .await?;
        let shutdown = CancellationToken::new();
        let elected = serve_elected_at(
            directory.path(),
            guard,
            shutdown.clone(),
            storage,
            rift_index::WorkspaceIndexLimits::default(),
            TokenCheck::Required,
            crate::identity::BuildCheckout::Unversioned,
        )
        .await?;
        assert!(
            document_path(directory.path()).exists(),
            "the election publishes before background discovery refuses its file bound"
        );
        shutdown.cancel();
        let (guard, _deadline, outcome) = elected.stopped(std::time::Duration::from_secs(30)).await;
        outcome?;
        drop(guard);
        assert!(!document_path(directory.path()).exists());
        Ok(())
    }

    /// The expected outcome of a start race is recorded where an operator looks for a
    /// refusal, at `INFO`, and the process still exits on it.
    #[test]
    fn a_lost_election_is_recorded_at_info() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        tracing::subscriber::with_default(subscriber, || {
            super::record_start_failure(&errors::mcp::election_already_serving().error());
        });
        let recorded = drain
            .try_recv_record()
            .expect("the lost election is recorded");
        assert_eq!(recorded.level(), "info");
        assert_eq!(recorded.operation(), "server.start");
        assert!(
            drain.try_recv_record().is_err(),
            "one record, no error event"
        );
    }

    #[test]
    fn served_document_records_this_process_and_release() {
        let token = "a".repeat(SERVER_TOKEN_LENGTH);
        let identity = valid_document().identity;
        let document = served_document(SERVER_PORT_MIN, &token, identity.clone(), None);
        assert_eq!(document.pid, std::process::id());
        assert_eq!(document.identity, identity);
        assert_eq!(document.port, SERVER_PORT_MIN);
        assert_eq!(document.token, token);
        assert_eq!(document.validate(), Ok(()));
    }

    #[test]
    fn election_errors_carry_registered_slug_evidence_and_source() {
        let storage = errors::mcp::election_storage_failed()
            .operation("open election file")
            .path(std::path::Path::new("/workspace/.rift/server.lock"))
            .source(std::io::Error::other("disk gone"))
            .error();
        assert_eq!(storage.slug(), errors::mcp::election_storage_failed::SLUG);
        assert!(storage.context().any(|(key, _)| key == "operation"));
        assert!(storage.context().any(|(key, _)| key == "path"));
        let rendered = storage.to_string();
        assert!(rendered.contains("open election file"), "{rendered}");
        assert!(rendered.contains("server.lock"), "{rendered}");
        assert!(
            std::error::Error::source(&storage).is_some(),
            "storage errors keep their io source"
        );
    }

    #[test]
    fn already_serving_error_carries_no_evidence_and_no_source() {
        let error = errors::mcp::election_already_serving().error();
        assert!(error.context().next().is_none());
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn document_invalid_error_carries_the_violation_evidence() {
        let mut document = valid_document();
        document.pid = 0;
        let violation = document
            .validate()
            .expect_err("a zero pid must break the contract");
        let error = super::election_document_invalid(violation);
        let context = error.context().collect::<Vec<_>>();
        assert!(
            context.iter().any(|(key, _)| *key == "pid"),
            "the violation's evidence must surface: {context:?}"
        );
        assert_eq!(error.slug(), errors::mcp::election_document_invalid::SLUG);
    }

    #[test]
    fn claim_refuses_when_a_file_obstructs_the_state_directory() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join(".rift"), b"a file in the way")?;
        let error = claim(directory.path()).expect_err("a file at .rift must refuse the claim");
        assert!(
            error
                .context()
                .any(|(key, value)| key == "operation" && value == "create state directory")
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn publish_reports_a_staging_failure() -> TestResult {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        let state_directory = directory.path().join(".rift");
        let saved = fs::metadata(&state_directory)?.permissions();
        fs::set_permissions(&state_directory, fs::Permissions::from_mode(0o500))?;
        let outcome = guard.publish(&valid_document());
        fs::set_permissions(&state_directory, saved)?;
        let error = outcome.expect_err("a read-only state directory must fail the staging");
        assert!(
            error
                .context()
                .any(|(key, value)| key == "operation" && value == "stage lock document")
        );
        Ok(())
    }

    #[test]
    fn publish_reports_a_persist_failure_over_an_obstructed_path() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        fs::create_dir_all(document_path(directory.path()))?;
        let error = guard
            .publish(&valid_document())
            .expect_err("a directory at the document path must fail the rename");
        assert!(
            error
                .context()
                .any(|(key, value)| key == "operation" && value == "publish lock document")
        );
        Ok(())
    }

    #[test]
    fn retire_reports_an_unremovable_document() -> TestResult {
        let directory = tempfile::tempdir()?;
        let guard = claim(directory.path())?;
        fs::create_dir_all(document_path(directory.path()))?;
        guard.retire();
        assert!(
            document_path(directory.path()).exists(),
            "a directory survives the best-effort removal"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn probe_classifies_an_unreadable_document_as_stale() -> TestResult {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let path = document_path(directory.path());
        fs::create_dir_all(path.parent().ok_or("document path must have a parent")?)?;
        fs::write(&path, serde_json::to_vec(&valid_document())?)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000))?;
        let presence = probe(directory.path());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        assert!(
            matches!(
                presence,
                ServerPresence::Stale(StaleReason::DocumentUnreadable)
            ),
            "an unreadable document must classify as stale: {presence:?}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn probe_classifies_an_unopenable_election_file_as_unobservable() -> TestResult {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let path = document_path(directory.path());
        fs::create_dir_all(path.parent().ok_or("document path must have a parent")?)?;
        fs::write(&path, serde_json::to_vec(&valid_document())?)?;
        let election_path = directory
            .path()
            .join(".rift")
            .join(SERVER_ELECTION_FILE_NAME);
        fs::write(&election_path, b"")?;
        fs::set_permissions(&election_path, fs::Permissions::from_mode(0o200))?;
        let presence = probe(directory.path());
        fs::set_permissions(&election_path, fs::Permissions::from_mode(0o600))?;
        assert!(
            matches!(
                presence,
                ServerPresence::Stale(StaleReason::ElectionUnobservable)
            ),
            "an unopenable election file must classify as unobservable: {presence:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn shut_down_unpublished_returns_the_fabricated_failure() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        let shutdown = CancellationToken::new();
        let serving_stop = shutdown.child_token();
        let server =
            serve_http(directory.path(), serving_stop.clone(), TokenCheck::Required).await?;
        let failure = errors::mcp::election_already_serving().error();
        let returned = shut_down_unpublished(server, &serving_stop, failure).await;
        assert_eq!(returned.slug(), errors::mcp::election_already_serving::SLUG);
        assert!(
            serving_stop.is_cancelled(),
            "the unpublished server's token must be cancelled"
        );
        Ok(())
    }

    #[tokio::test]
    async fn supplied_storage_requires_its_own_election_before_serving() -> TestResult {
        let requested = tempfile::tempdir()?;
        let other = tempfile::tempdir()?;
        let guard = Arc::new(claim(requested.path())?);
        let other_guard = Arc::new(claim(other.path())?);
        let storage = crate::WorkspaceStorage::open_elected(other.path(), other_guard).await?;
        let error = super::serve_elected_with_storage(
            requested.path(),
            guard,
            CancellationToken::new(),
            storage,
            TokenCheck::Required,
            crate::identity::BuildCheckout::Unversioned,
        )
        .await
        .expect_err("another workspace's storage cannot serve");
        assert!(
            error
                .context()
                .any(|(key, value)| key == "operation" && value == "validate workspace storage")
        );
        assert!(!document_path(requested.path()).exists());
        assert!(!requested.path().join(".rift/db").exists());
        Ok(())
    }

    #[tokio::test]
    async fn a_lost_elected_start_does_not_open_workspace_storage() -> TestResult {
        let directory = tempfile::tempdir()?;
        let _guard = claim(directory.path())?;
        let error = serve_elected(directory.path(), CancellationToken::new())
            .await
            .expect_err("a competing owner refuses the start");
        assert_eq!(error.slug(), errors::mcp::election_already_serving::SLUG);
        assert!(!directory.path().join(".rift/db").exists());
        Ok(())
    }

    #[tokio::test]
    async fn serve_elected_shuts_down_when_the_publish_fails() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        fs::create_dir_all(document_path(directory.path()))?;
        let shutdown = CancellationToken::new();
        let error = serve_elected(directory.path(), shutdown)
            .await
            .expect_err("publishing over a directory must fail the election");
        assert!(
            error
                .context()
                .any(|(key, value)| key == "operation" && value == "publish lock document")
        );
        Ok(())
    }

    #[tokio::test]
    async fn the_document_outlives_the_transport_stop_and_the_caller_retires_it() -> TestResult {
        let directory = tempfile::tempdir()?;
        crate::server::hermetic_workspace(directory.path(), "")?;
        let shutdown = CancellationToken::new();
        let elected = serve_elected(directory.path(), shutdown.clone()).await?;
        let document = document_path(directory.path());
        assert!(
            document.is_file(),
            "a served election publishes its document"
        );

        shutdown.cancel();
        let (guard, _deadline, outcome) = elected.stopped(std::time::Duration::from_secs(8)).await;
        outcome.map_err(|error| format!("the transport must stop cleanly: {error:?}"))?;
        assert!(
            document.is_file(),
            "the document outlives the transport stop; only the caller retires it"
        );

        guard.retire();
        assert!(
            !document.exists(),
            "the caller's retire removes the document"
        );
        Ok(())
    }
}
