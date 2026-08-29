//! Putting one embedding model's files in the machine's Hugging Face cache.
//!
//! The cache is the directory `huggingface-cli` and `huggingface_hub` already
//! use, resolved from the same variables in the same order, so a model another
//! client downloaded is a hit here and a model this module downloaded is a hit
//! there. Rift never opens a second cache directory of its own.
//!
//! This module is the crate's only async surface and its only network surface.
//! Everything else in the crate runs synchronously and touches neither.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rift_core::constants::{HTTPS_SCHEME, REDIRECT_HOPS_MAX};
use tokio::io::AsyncWriteExt as _;

use crate::encoder::{CONFIGURATION_FILE, ModelFiles, TOKENIZER_FILE, WEIGHTS_FILE};
use crate::error::{SearchError, SearchFault, SearchViolation};

/// The variable naming the hub root directly.
const HUB_CACHE_VARIABLE: &str = "HF_HUB_CACHE";
/// The variable naming the Hugging Face home the hub root sits below.
const HUB_HOME_VARIABLE: &str = "HF_HOME";
/// The variable naming the directory a user's caches sit below.
const CACHE_HOME_VARIABLE: &str = "XDG_CACHE_HOME";
/// The variable naming the origin the files are read from.
const ENDPOINT_VARIABLE: &str = "HF_ENDPOINT";
/// The variable holding the user's home directory on unix.
const USER_HOME_VARIABLE: &str = "HOME";
/// The variable holding the user's home directory on Windows.
const USER_PROFILE_VARIABLE: &str = "USERPROFILE";

/// The directory the hub root occupies below a Hugging Face home.
const HUB_DIRECTORY: &str = "hub";
/// The directory every Hugging Face client caches below a cache home.
const HUGGINGFACE_DIRECTORY: &str = "huggingface";
/// The directory a home directory holds its caches in.
const HOME_CACHE_DIRECTORY: &str = ".cache";

/// The prefix one model repository's directory carries below the hub root.
const REPOSITORY_DIRECTORY_PREFIX: &str = "models--";
/// What a `/` becomes in a repository's directory name.
const REPOSITORY_SEGMENT_SEPARATOR: &str = "--";
/// The directory holding one file per revision, each naming a commit.
const REFS_DIRECTORY: &str = "refs";
/// The directory holding one file per etag, each holding that file's content.
const BLOBS_DIRECTORY: &str = "blobs";
/// The directory holding one directory per commit, each naming the files.
const SNAPSHOTS_DIRECTORY: &str = "snapshots";
/// The suffix a write in progress carries until it is renamed into place.
const INCOMPLETE_SUFFIX: &str = "incomplete";

/// The origin every Hugging Face client reads from when no variable names one.
const HUB_ENDPOINT: &str = "https://huggingface.co";
/// The path segment one file of one revision is served under.
const RESOLVE_SEGMENT: &str = "resolve";
/// The header an origin declares the commit a revision resolved to in.
const COMMIT_HEADER: &str = "x-repo-commit";
/// The prefix a weak etag carries before its bare value.
const WEAK_ETAG_PREFIX: &str = "W/";
/// The quote a strong etag is wrapped in.
const ETAG_QUOTE: char = '"';
/// The name this crate identifies itself by to the origin.
const ACQUISITION_USER_AGENT: &str = "rift-search";

/// The separator a repository identifier puts between owner and name.
const PATH_SEPARATOR: char = '/';
/// The separator a model identifier puts before its revision.
const REVISION_SEPARATOR: char = '@';
/// The revision read when a model identifier names none.
const DEFAULT_REVISION: &str = "main";

/// Maximum bytes in one model `config.json`, which holds a few dozen fields.
const CONFIGURATION_BYTES_MAX: u64 = 1_024 * 1_024;
/// Maximum bytes in one model `tokenizer.json`, which holds a vocabulary of
/// tens of thousands of entries with its merges.
const TOKENIZER_BYTES_MAX: u64 = 16 * 1_024 * 1_024;
/// Maximum bytes in one model weights file.
///
/// Every other download ceiling in this workspace tops out at 128MB, and this
/// one may not copy that number: `BAAI/bge-small-en-v1.5` ships a
/// `model.safetensors` of roughly 133MB, so a 128MB bound would refuse the
/// default model on its first download.
const WEIGHTS_BYTES_MAX: u64 = 512 * 1_024 * 1_024;

/// What one retry delay is multiplied by for each attempt already made.
///
/// The formula is `RetryPolicy::delay_after` in `rift-protocol`, re-applied
/// here: neither `rift-index` nor this crate depends on the protocol crate, and
/// adding that edge to reuse one function would break the layering.
const RETRY_GROWTH_FACTOR: u64 = 2;

/// One file an encoder loads, and the ceiling its download honours.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ModelFile {
    name: &'static str,
    bytes_max: u64,
}

impl ModelFile {
    /// Names one file and the bytes its download may spend.
    const fn new(name: &'static str, bytes_max: u64) -> Self {
        Self { name, bytes_max }
    }
}

/// The three files an encoder loads. The first fetched fixes the snapshot the
/// other two are placed in, so the three are one revision of the repository.
const MODEL_FILES: [ModelFile; 3] = [
    ModelFile::new(CONFIGURATION_FILE, CONFIGURATION_BYTES_MAX),
    ModelFile::new(TOKENIZER_FILE, TOKENIZER_BYTES_MAX),
    ModelFile::new(WEIGHTS_FILE, WEIGHTS_BYTES_MAX),
];

/// One resolved model: where its weights come from, and what it names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelSource {
    /// One Hugging Face repository at one revision.
    Repository {
        /// The `owner/name` the repository is addressed by.
        repository: String,
        /// The branch, tag, or commit the files are read at.
        revision: String,
    },
    /// One directory below the workspace root.
    Directory(PathBuf),
}

impl ModelSource {
    /// Reads `model` as a hub repository identifier: `owner/name`, or
    /// `owner/name@revision`. The default revision is `main`.
    ///
    /// # Errors
    ///
    /// Returns `model_source_invalid` naming `model` and the form that was
    /// expected, so an operator learns what to write rather than that the value
    /// was rejected.
    pub fn repository(model: &str) -> Result<Self, SearchError> {
        match repository_violation(model) {
            Some(expected) => Err(model_source_invalid(model, expected)),
            None => Ok(Self::at_revision(model)),
        }
    }

    /// The repository and revision one accepted identifier names.
    fn at_revision(model: &str) -> Self {
        let (repository, revision) = split_model(model);
        Self::Repository {
            repository: repository.to_owned(),
            revision: revision.to_owned(),
        }
    }

    /// Reads `model` as a workspace-relative directory, resolved against
    /// `root`.
    ///
    /// # Errors
    ///
    /// Returns `model_source_invalid` naming `model` and the form that was
    /// expected, so an operator learns what to write rather than that the value
    /// was rejected.
    pub fn directory(model: &str, root: &Path) -> Result<Self, SearchError> {
        match relative_path_violation(model) {
            Some(expected) => Err(model_source_invalid(model, expected)),
            None => Ok(Self::Directory(root.join(model))),
        }
    }
}

/// The repository and revision halves of one model identifier.
fn split_model(model: &str) -> (&str, &str) {
    match model.split_once(REVISION_SEPARATOR) {
        Some((repository, revision)) => (repository, revision),
        None => (model, DEFAULT_REVISION),
    }
}

/// Whether one path segment names its own directory or its parent.
fn is_dot_segment(segment: &str) -> bool {
    segment == "." || segment == ".."
}

/// What a Hugging Face model identifier broke, in precedence order.
fn repository_violation(model: &str) -> Option<&'static str> {
    let (repository, revision) = split_model(model);
    let segments: Vec<&str> = repository.split(PATH_SEPARATOR).collect();
    match segments.as_slice() {
        _ if model.matches(REVISION_SEPARATOR).count() > 1 => {
            Some("one `@` at most, as in `owner/name@revision`")
        }
        [owner, name] if owner.is_empty() || name.is_empty() => {
            Some("a non-empty owner and name, as in `BAAI/bge-small-en-v1.5`")
        }
        [_, _] if revision.is_empty() => Some("a non-empty revision after `@`"),
        [_, _] if revision.contains(PATH_SEPARATOR) => Some("a revision carrying no `/`"),
        [owner, name] if [*owner, *name, revision].into_iter().any(is_dot_segment) => {
            Some("no `.` or `..` segment")
        }
        [_, _] => None,
        _ => Some("the form `owner/name`, as in `BAAI/bge-small-en-v1.5`"),
    }
}

/// What a model directory path broke, in precedence order.
fn relative_path_violation(value: &str) -> Option<&'static str> {
    match value.as_bytes() {
        [] => Some("a non-empty path"),
        [PATH_SEPARATOR_BYTE, ..] => Some("a path relative to the workspace root"),
        [drive, b':', ..] if drive.is_ascii_alphabetic() => {
            Some("a path relative to the workspace root")
        }
        bytes if bytes.contains(&b'\\') => Some("`/` as the separator"),
        _ if value.chars().any(char::is_control) => Some("no control character"),
        _ if value.split(PATH_SEPARATOR).any(str::is_empty) => Some("no empty segment"),
        _ if value.split(PATH_SEPARATOR).any(is_dot_segment) => Some("no `.` or `..` segment"),
        _ => None,
    }
}

/// The separator as one byte, so a leading separator is a slice pattern.
const PATH_SEPARATOR_BYTE: u8 = b'/';

/// What one acquisition may spend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcquisitionLimits {
    timeout: Duration,
    attempts: u32,
    retry_delay: Duration,
    retry_delay_limit: Duration,
}

impl AcquisitionLimits {
    /// Bounds one acquisition: the wait per request, and the retry budget.
    ///
    /// This crate declares its own limits type and the server translates
    /// configuration into it, the way `rift-index` already does, so the search
    /// tier stays independent of the protocol crate.
    #[must_use]
    pub const fn new(
        timeout: Duration,
        attempts: u32,
        retry_delay: Duration,
        retry_delay_limit: Duration,
    ) -> Self {
        Self {
            timeout,
            attempts,
            retry_delay,
            retry_delay_limit,
        }
    }

    /// The wall-clock limit one request runs under.
    #[must_use]
    pub const fn timeout(self) -> Duration {
        self.timeout
    }

    /// The fetches one file may cost, the first included.
    #[must_use]
    pub const fn attempts(self) -> u32 {
        self.attempts
    }

    /// The wait before the second attempt.
    #[must_use]
    pub const fn retry_delay(self) -> Duration {
        self.retry_delay
    }

    /// The wait no retry delay grows past.
    #[must_use]
    pub const fn retry_delay_limit(self) -> Duration {
        self.retry_delay_limit
    }

    /// The wait before the attempt after `attempt`, absent once the attempt
    /// bound is spent.
    ///
    /// Attempts are numbered from one, so `delay_after(1)` is the wait before
    /// the second attempt and answers `retry_delay`. The wait is `retry_delay`
    /// times `RETRY_GROWTH_FACTOR` raised to the attempts already made, held
    /// at `retry_delay_limit`. Growth that would overflow saturates, and the
    /// ceiling clamps the saturated value.
    #[must_use]
    pub fn delay_after(self, attempt: u32) -> Option<Duration> {
        if attempt >= self.attempts {
            return None;
        }
        let made = attempt.saturating_sub(1);
        let grown = RETRY_GROWTH_FACTOR
            .checked_pow(made)
            .and_then(|growth| milliseconds(self.retry_delay).checked_mul(growth))
            .unwrap_or(u64::MAX);
        Some(Duration::from_millis(
            grown.min(milliseconds(self.retry_delay_limit)),
        ))
    }
}

/// One duration in whole milliseconds, saturating past what a `u64` holds.
fn milliseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// What one fetch produced.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FetchedFile {
    commit: Option<String>,
    etag: Option<String>,
}

impl FetchedFile {
    /// Names the commit and the etag one origin declared.
    #[must_use]
    pub const fn new(commit: Option<String>, etag: Option<String>) -> Self {
        Self { commit, etag }
    }

    /// The commit the origin declared, absent when it declared none.
    #[must_use]
    pub fn commit(&self) -> Option<&str> {
        self.commit.as_deref()
    }

    /// The etag the origin declared, reduced to its bare value.
    #[must_use]
    pub fn etag(&self) -> Option<&str> {
        self.etag.as_deref()
    }

    /// The commit and etag one response declared in its headers.
    fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        Self {
            commit: header_text(headers, COMMIT_HEADER),
            etag: header_text(headers, reqwest::header::ETAG.as_str())
                .map(|value| bare_etag(&value)),
        }
    }
}

/// One header's value as text, absent when it is missing or not ASCII.
fn header_text(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// One etag with its weak prefix and its quotes removed.
fn bare_etag(value: &str) -> String {
    value
        .strip_prefix(WEAK_ETAG_PREFIX)
        .unwrap_or(value)
        .trim_matches(ETAG_QUOTE)
        .to_owned()
}

/// Fetches one file, reporting the commit and etag the origin declared.
trait FileTransport {
    /// Writes the body at `url` to `destination`, spending at most `bytes_max`.
    async fn fetch(
        &self,
        url: &str,
        destination: &Path,
        bytes_max: u64,
    ) -> Result<FetchedFile, SearchError>;
}

/// The client one acquisition fetches through.
struct HubTransport {
    client: reqwest::Client,
}

impl HubTransport {
    /// Builds a client that follows only https redirects, within a hop bound.
    fn new(timeout: Duration) -> Result<Self, SearchError> {
        let redirects = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= REDIRECT_HOPS_MAX
                || attempt.url().scheme() != HTTPS_SCHEME
            {
                attempt.stop()
            } else {
                attempt.follow()
            }
        });
        let client = reqwest::Client::builder()
            .redirect(redirects)
            .timeout(timeout)
            .min_tls_version(reqwest::tls::Version::TLS_1_2)
            .user_agent(ACQUISITION_USER_AGENT)
            .build()
            .map_err(download_failure)?;
        Ok(Self { client })
    }
}

impl FileTransport for HubTransport {
    async fn fetch(
        &self,
        url: &str,
        destination: &Path,
        bytes_max: u64,
    ) -> Result<FetchedFile, SearchError> {
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(download_failure)?;
        if response
            .content_length()
            .is_some_and(|length| length > bytes_max)
        {
            return Err(download_too_large(url, bytes_max));
        }
        let declared = FetchedFile::from_headers(response.headers());
        let mut output = tokio::fs::File::create(destination)
            .await
            .map_err(download_failure)?;
        let received = write_bounded_body(&mut response, &mut output, bytes_max).await?;
        if received == 0 || received > bytes_max {
            return Err(download_too_large(url, bytes_max));
        }
        output.sync_all().await.map_err(download_failure)?;
        Ok(declared)
    }
}

/// Streams a response body to `output`, counting at most one byte past
/// `bytes_max`.
///
/// The loop is bounded by that ceiling: every pass either adds at least one
/// counted byte or ends the body, so an oversized body is detected without
/// being drained or held whole.
async fn write_bounded_body(
    response: &mut reqwest::Response,
    output: &mut tokio::fs::File,
    bytes_max: u64,
) -> Result<u64, SearchError> {
    let ceiling = bytes_max.saturating_add(1);
    let mut received = 0_u64;
    while received < ceiling {
        let Some(chunk) = response.chunk().await.map_err(download_failure)? else {
            break;
        };
        let kept = bytes_within_budget(chunk.len(), ceiling - received);
        output
            .write_all(&chunk[..kept])
            .await
            .map_err(download_failure)?;
        received = received.saturating_add(u64::try_from(kept).unwrap_or(u64::MAX));
    }
    Ok(received)
}

/// Bytes of one body chunk that fit within the remaining counted budget.
fn bytes_within_budget(chunk_length: usize, budget: u64) -> usize {
    usize::try_from(budget).map_or(chunk_length, |budget| chunk_length.min(budget))
}

/// Where the machine's Hugging Face cache is, and which origin serves it.
///
/// The variables are read once, here, so the resolution below is a function of
/// these five values and nothing else.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct HubEnvironment {
    hub_cache: Option<PathBuf>,
    hub_home: Option<PathBuf>,
    cache_home: Option<PathBuf>,
    user_home: Option<PathBuf>,
    endpoint: Option<String>,
}

impl HubEnvironment {
    /// Reads the variables from this process.
    fn from_process() -> Self {
        Self::read(&|name| std::env::var_os(name))
    }

    /// Reads the variables through `lookup`.
    ///
    /// An empty value counts as unset, which is what every Hugging Face client
    /// does. The home directory is read from the environment rather than from
    /// the account database, so the resolution is total and stays a function of
    /// its inputs.
    fn read(lookup: &dyn Fn(&str) -> Option<OsString>) -> Self {
        let set = |name: &str| lookup(name).filter(|value| !value.is_empty());
        Self {
            hub_cache: set(HUB_CACHE_VARIABLE).map(PathBuf::from),
            hub_home: set(HUB_HOME_VARIABLE).map(PathBuf::from),
            cache_home: set(CACHE_HOME_VARIABLE).map(PathBuf::from),
            user_home: set(USER_HOME_VARIABLE)
                .or_else(|| set(USER_PROFILE_VARIABLE))
                .map(PathBuf::from),
            endpoint: set(ENDPOINT_VARIABLE).and_then(|value| value.into_string().ok()),
        }
    }

    /// The hub root, in the precedence every Hugging Face client applies.
    ///
    /// # Errors
    ///
    /// Returns `model_cache_unavailable` when no variable and no home directory
    /// names a root.
    fn cache_root(&self) -> Result<PathBuf, SearchError> {
        self.hub_cache
            .clone()
            .or_else(|| self.hub_home.as_ref().map(|home| home.join(HUB_DIRECTORY)))
            .or_else(|| {
                self.cache_home
                    .as_ref()
                    .map(|cache| cache.join(HUGGINGFACE_DIRECTORY).join(HUB_DIRECTORY))
            })
            .or_else(|| {
                self.user_home.as_ref().map(|home| {
                    home.join(HOME_CACHE_DIRECTORY)
                        .join(HUGGINGFACE_DIRECTORY)
                        .join(HUB_DIRECTORY)
                })
            })
            .ok_or_else(cache_unavailable)
    }

    /// The origin one file is read from.
    fn endpoint(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(HUB_ENDPOINT)
    }
}

/// Where one repository revision's files are read from.
struct RepositoryOrigin {
    endpoint: String,
    repository: String,
    revision: String,
}

impl RepositoryOrigin {
    /// Names the origin, the repository, and the revision one file is read at.
    fn new(endpoint: &str, repository: &str, revision: &str) -> Self {
        Self {
            endpoint: endpoint.to_owned(),
            repository: repository.to_owned(),
            revision: revision.to_owned(),
        }
    }

    /// The revision the files are read at.
    fn revision(&self) -> &str {
        &self.revision
    }

    /// The URL one file of this revision is served at.
    fn file_url(&self, file: &str) -> String {
        format!(
            "{}/{}/{RESOLVE_SEGMENT}/{}/{file}",
            self.endpoint.trim_end_matches(PATH_SEPARATOR),
            self.repository,
            self.revision
        )
    }
}

/// The directory one repository occupies below the hub root.
fn repository_directory(repository: &str) -> String {
    format!(
        "{REPOSITORY_DIRECTORY_PREFIX}{}",
        repository.replace(PATH_SEPARATOR, REPOSITORY_SEGMENT_SEPARATOR)
    )
}

/// One downloaded file: the commit its origin declared, and the blob holding it.
#[derive(Clone, Debug, Eq, PartialEq)]
struct FetchedBlob {
    commit: String,
    blob: PathBuf,
}

/// Where one repository's files sit below the hub root.
struct RepositoryCache {
    directory: PathBuf,
}

impl RepositoryCache {
    /// The directory `repository` occupies below `root`.
    fn new(root: &Path, repository: &str) -> Self {
        Self {
            directory: root.join(repository_directory(repository)),
        }
    }

    /// The directory holding one file per revision.
    fn refs(&self) -> PathBuf {
        self.directory.join(REFS_DIRECTORY)
    }

    /// The directory holding one file per etag.
    fn blobs(&self) -> PathBuf {
        self.directory.join(BLOBS_DIRECTORY)
    }

    /// The directory one commit's files are addressed by name in.
    fn snapshot(&self, commit: &str) -> PathBuf {
        self.directory.join(SNAPSHOTS_DIRECTORY).join(commit)
    }

    /// The snapshot this cache already holds every model file in.
    ///
    /// A hit is any existing entry: `huggingface_hub` links a snapshot name to
    /// its blob where it can and copies where it cannot, and `Path::exists`
    /// answers true for both. A commit that would address a path outside this
    /// repository is no hit, so a damaged `refs` file cannot steer a read.
    fn cached_snapshot(&self, revision: &str) -> Option<PathBuf> {
        let recorded = std::fs::read_to_string(self.refs().join(revision)).ok()?;
        let commit = recorded.trim();
        if segment_violation(commit).is_some() {
            return None;
        }
        let snapshot = self.snapshot(commit);
        MODEL_FILES
            .iter()
            .all(|file| snapshot.join(file.name).exists())
            .then_some(snapshot)
    }

    /// Fetches one file and puts its content in this cache's blobs.
    ///
    /// The staged name lives beside the blob it becomes, so the rename that
    /// finishes it is atomic, and a fetch that failed or was cancelled leaves
    /// nothing a later run reads.
    async fn fetch_blob<T: FileTransport>(
        &self,
        origin: &RepositoryOrigin,
        file: ModelFile,
        limits: AcquisitionLimits,
        transport: &T,
    ) -> Result<FetchedBlob, SearchError> {
        let blobs = self.blobs();
        tokio::fs::create_dir_all(&blobs)
            .await
            .map_err(download_failure)?;
        let staged = blobs.join(staged_name(file.name));
        let url = origin.file_url(file.name);
        let accepted = fetch_with_retry(transport, &url, &staged, file.bytes_max, limits)
            .await
            .and_then(|fetched| accepted_blob(&fetched, origin.revision(), &url, &blobs));
        let accepted = match accepted {
            Ok(accepted) => accepted,
            Err(failure) => return Err(discard(&staged, failure).await),
        };
        place_blob(&staged, &accepted.blob).await?;
        Ok(accepted)
    }
}

/// The commit and the blob path one fetch's declared headers address.
///
/// The commit falls back to the revision when the origin declared none: a
/// mirror that serves the files without the header still addresses one
/// snapshot, and the revision is the only name both sides already agree on.
fn accepted_blob(
    fetched: &FetchedFile,
    revision: &str,
    url: &str,
    blobs: &Path,
) -> Result<FetchedBlob, SearchError> {
    let commit = accepted_segment(fetched.commit().unwrap_or(revision))?;
    let etag = fetched
        .etag()
        .ok_or_else(|| download_refused(format!("`{url}`: expected an `etag` header")))?;
    Ok(FetchedBlob {
        commit,
        blob: blobs.join(accepted_segment(etag)?),
    })
}

/// What one origin-supplied name broke, in precedence order.
fn segment_violation(value: &str) -> Option<&'static str> {
    match value.as_bytes() {
        [] => Some("a non-empty value"),
        bytes if bytes.contains(&PATH_SEPARATOR_BYTE) || bytes.contains(&b'\\') => {
            Some("no path separator")
        }
        _ if is_dot_segment(value) => Some("no `.` or `..` segment"),
        _ => None,
    }
}

/// One origin-supplied value accepted as a name below the cache.
///
/// The commit names a snapshot directory and the etag names a blob, so both
/// become path segments. A value carrying a separator or a dot segment would
/// address a path outside the repository, and is refused before any path is
/// built from it.
fn accepted_segment(value: &str) -> Result<String, SearchError> {
    match segment_violation(value) {
        Some(expected) => Err(download_refused(format!("`{value}`: expected {expected}"))),
        None => Ok(value.to_owned()),
    }
}

/// Fetches one file, retrying a failure while the attempt bound allows.
///
/// The loop runs at most `limits.attempts()` times: `delay_after` answers
/// `None` once the bound is spent, which is the only way out other than a
/// success. The failure returned is the last one the transport reported, not a
/// synthetic one.
async fn fetch_with_retry<T: FileTransport>(
    transport: &T,
    url: &str,
    destination: &Path,
    bytes_max: u64,
    limits: AcquisitionLimits,
) -> Result<FetchedFile, SearchError> {
    let mut attempt = 1_u32;
    loop {
        let failure = match transport.fetch(url, destination, bytes_max).await {
            Ok(fetched) => return Ok(fetched),
            Err(failure) => failure,
        };
        let Some(delay) = limits.delay_after(attempt) else {
            let attempts = limits.attempts();
            tracing::warn!(
                component = "search",
                subject = url,
                attempts,
                "model download failed on every attempt"
            );
            return Err(failure);
        };
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

/// A name no other write uses, in the directory the finished file lands in.
///
/// The process id and a counter make it unique across processes and calls, so
/// two acquisitions of one model cannot overwrite each other's staged bytes.
fn staged_name(name: &str) -> String {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let sequence = WRITES.fetch_add(1, Ordering::Relaxed);
    format!(
        "{name}.{}.{sequence}.{INCOMPLETE_SUFFIX}",
        std::process::id()
    )
}

/// Removes what a failed fetch staged and keeps the failure that caused it.
///
/// The removal is best effort: the staged name is one this process made, and a
/// name no later run reads either way.
async fn discard(staged: &Path, failure: SearchError) -> SearchError {
    let _ = tokio::fs::remove_file(staged).await;
    failure
}

/// Renames staged content onto its blob, keeping a blob already there.
///
/// The cache belongs to every Hugging Face client on the machine, so a blob
/// another client wrote is never rewritten.
async fn place_blob(staged: &Path, blob: &Path) -> Result<(), SearchError> {
    if blob.exists() {
        let _ = tokio::fs::remove_file(staged).await;
        return Ok(());
    }
    rename_or_discard(staged, blob).await
}

/// Places one blob at its addressable name in `snapshot`.
///
/// A hard link is tried first: it costs no second copy of the bytes. A copy
/// follows when linking fails, which is what `huggingface_hub` does on a
/// filesystem that has no links. A symlink is never used, because creating one
/// needs privileges on Windows.
async fn place_in_snapshot(blob: &Path, snapshot: &Path, name: &str) -> Result<(), SearchError> {
    let destination = snapshot.join(name);
    if destination.exists() {
        return Ok(());
    }
    tokio::fs::create_dir_all(snapshot)
        .await
        .map_err(download_failure)?;
    if tokio::fs::hard_link(blob, &destination).await.is_ok() {
        return Ok(());
    }
    copy_atomically(blob, snapshot, name).await
}

/// Copies `blob` to `name` in `directory` through a staged name there.
async fn copy_atomically(blob: &Path, directory: &Path, name: &str) -> Result<(), SearchError> {
    let staged = directory.join(staged_name(name));
    tokio::fs::copy(blob, &staged)
        .await
        .map_err(download_failure)?;
    rename_or_discard(&staged, &directory.join(name)).await
}

/// Writes `bytes` to `name` in `directory` through a staged name there.
async fn write_atomically(directory: &Path, name: &str, bytes: &[u8]) -> Result<(), SearchError> {
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(download_failure)?;
    let staged = directory.join(staged_name(name));
    tokio::fs::write(&staged, bytes)
        .await
        .map_err(download_failure)?;
    rename_or_discard(&staged, &directory.join(name)).await
}

/// Renames a staged file into place, removing it when the rename fails.
async fn rename_or_discard(staged: &Path, destination: &Path) -> Result<(), SearchError> {
    match tokio::fs::rename(staged, destination).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = tokio::fs::remove_file(staged).await;
            Err(download_failure(error))
        }
    }
}

/// Puts the three files a BERT checkpoint needs in the machine's Hugging Face
/// cache and returns the loaded file set.
///
/// A [`ModelSource::Directory`] touches neither the network nor the cache. A
/// [`ModelSource::Repository`] already in the cache touches no network either:
/// the snapshot the revision resolves to is read as it stands.
///
/// # Errors
///
/// Returns `model_cache_unavailable` when no cache root resolves,
/// `model_download_failed` when every attempt at one file failed,
/// `model_download_too_large` when a body ran past its ceiling, and
/// `model_file_missing` when a directory holds none of the three files.
///
/// # Cancel safety
///
/// Cancelling this call leaves the cache as it was found or further along it:
/// every write lands under a staged name in its own directory and is renamed
/// into place, so a cancelled fetch leaves no file a later run reads.
pub async fn acquire(
    source: &ModelSource,
    limits: AcquisitionLimits,
) -> Result<ModelFiles, SearchError> {
    acquire_from(source, limits, &HubEnvironment::from_process()).await
}

/// Acquires one model against `environment` rather than the process.
async fn acquire_from(
    source: &ModelSource,
    limits: AcquisitionLimits,
    environment: &HubEnvironment,
) -> Result<ModelFiles, SearchError> {
    match source {
        ModelSource::Directory(directory) => ModelFiles::in_directory(directory),
        ModelSource::Repository {
            repository,
            revision,
        } => {
            let origin = RepositoryOrigin::new(environment.endpoint(), repository, revision);
            let transport = HubTransport::new(limits.timeout())?;
            let root = environment.cache_root()?;
            acquire_repository(&origin, limits, &transport, &root).await
        }
    }
}

/// Puts one repository revision's three files in the cache below `root`.
///
/// The first file's commit fixes the snapshot the other two are placed in, so
/// the three an encoder loads are one revision of the repository. The revision
/// file is written last: until it names the commit, a later run sees a miss and
/// fetches again rather than reading a snapshot that is short a file.
async fn acquire_repository<T: FileTransport>(
    origin: &RepositoryOrigin,
    limits: AcquisitionLimits,
    transport: &T,
    root: &Path,
) -> Result<ModelFiles, SearchError> {
    let cache = RepositoryCache::new(root, &origin.repository);
    if let Some(snapshot) = cache.cached_snapshot(origin.revision()) {
        return ModelFiles::in_directory(&snapshot);
    }
    let [configuration, joining @ ..] = MODEL_FILES;
    let fetched = cache
        .fetch_blob(origin, configuration, limits, transport)
        .await?;
    let snapshot = cache.snapshot(&fetched.commit);
    place_in_snapshot(&fetched.blob, &snapshot, configuration.name).await?;
    for file in joining {
        let joined = cache.fetch_blob(origin, file, limits, transport).await?;
        place_in_snapshot(&joined.blob, &snapshot, file.name).await?;
    }
    write_atomically(&cache.refs(), origin.revision(), fetched.commit.as_bytes()).await?;
    ModelFiles::in_directory(&snapshot)
}

/// One model identifier refusal, naming the value and the form expected.
fn model_source_invalid(model: &str, expected: &str) -> SearchError {
    SearchError::new(
        SearchFault::new(SearchViolation::ModelSourceInvalid)
            .about(format!("`{model}`: expected {expected}")),
    )
}

/// The refusal a machine with no resolvable cache root earns.
fn cache_unavailable() -> SearchError {
    SearchError::new(SearchFault::new(SearchViolation::ModelCacheUnavailable).about(format!(
        "no Hugging Face cache directory resolved: set `{HUB_CACHE_VARIABLE}`, `{HUB_HOME_VARIABLE}`, `{CACHE_HOME_VARIABLE}`, or `{USER_HOME_VARIABLE}`"
    )))
}

/// One download failure, naming what the transport or the filesystem reported.
fn download_failure(source: impl std::error::Error + Send + Sync + 'static) -> SearchError {
    let subject = source.to_string();
    SearchError::new(
        SearchFault::new(SearchViolation::ModelDownloadFailed)
            .about(subject)
            .caused_by(source),
    )
}

/// One download refusal with no failure of its own to carry.
fn download_refused(subject: impl Into<String>) -> SearchError {
    SearchError::new(SearchFault::new(SearchViolation::ModelDownloadFailed).about(subject))
}

/// The refusal a body that was empty or ran past its ceiling earns.
fn download_too_large(url: &str, bytes_max: u64) -> SearchError {
    SearchError::new(
        SearchFault::new(SearchViolation::ModelDownloadTooLarge).about(format!(
            "`{url}` was empty or exceeded {bytes_max} bytes: expected a body within the ceiling this file is fetched under"
        )),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use super::{
        AcquisitionLimits, CACHE_HOME_VARIABLE, COMMIT_HEADER, CONFIGURATION_FILE,
        DEFAULT_REVISION, ENDPOINT_VARIABLE, FetchedFile, FileTransport, HUB_CACHE_VARIABLE,
        HUB_ENDPOINT, HUB_HOME_VARIABLE, HubEnvironment, HubTransport, MODEL_FILES, ModelSource,
        REFS_DIRECTORY, RepositoryCache, RepositoryOrigin, USER_HOME_VARIABLE,
        USER_PROFILE_VARIABLE, acquire_from, acquire_repository, bare_etag, copy_atomically,
        download_failure, download_refused, place_blob, place_in_snapshot, repository_directory,
        write_atomically,
    };
    use crate::encoder::ModelFiles;
    use crate::error::{SearchError, SearchViolation};

    type Fallible<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
    type TestResult = Fallible<()>;

    const REPOSITORY: &str = "BAAI/bge-small-en-v1.5";
    const REPOSITORY_DIRECTORY: &str = "models--BAAI--bge-small-en-v1.5";
    const COMMIT: &str = "0e0ac2cbe0ee1c1d";
    const ETAG: &str = "d41d8cd98f00b204";

    /// Limits that spend no wall clock, so a retry test costs nothing.
    fn limits(attempts: u32) -> AcquisitionLimits {
        AcquisitionLimits::new(
            Duration::from_secs(5),
            attempts,
            Duration::ZERO,
            Duration::ZERO,
        )
    }

    /// An environment built from `pairs` alone.
    ///
    /// No test reads the process environment into a cache root, so no test can
    /// address the machine's own Hugging Face cache.
    fn environment(pairs: &[(&str, &str)]) -> HubEnvironment {
        let held: HashMap<String, OsString> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(*value)))
            .collect();
        HubEnvironment::read(&|name| held.get(name).cloned())
    }

    /// A transport answering from bytes the test holds.
    struct FixtureTransport {
        content: Vec<u8>,
        commit: Option<String>,
        etag: Option<String>,
        failures: u32,
        calls: AtomicU32,
    }

    impl FixtureTransport {
        fn new(commit: Option<&str>, etag: Option<&str>) -> Self {
            Self {
                content: b"model bytes".to_vec(),
                commit: commit.map(str::to_owned),
                etag: etag.map(str::to_owned),
                failures: 0,
                calls: AtomicU32::new(0),
            }
        }

        fn failing(failures: u32) -> Self {
            Self {
                failures,
                ..Self::new(Some(COMMIT), Some(ETAG))
            }
        }

        fn calls(&self) -> u32 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl FileTransport for FixtureTransport {
        async fn fetch(
            &self,
            url: &str,
            destination: &Path,
            _bytes_max: u64,
        ) -> Result<FetchedFile, SearchError> {
            let call = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
            if call <= self.failures {
                return Err(download_refused(format!("`{url}`: fixture failure {call}")));
            }
            tokio::fs::write(destination, &self.content)
                .await
                .map_err(download_failure)?;
            Ok(FetchedFile::new(self.commit.clone(), self.etag.clone()))
        }
    }

    /// A transport that fails whenever it is asked, so a fetch a cache hit
    /// should have avoided shows up as a refusal.
    struct RefusingTransport;

    impl FileTransport for RefusingTransport {
        fn fetch(
            &self,
            url: &str,
            _destination: &Path,
            _bytes_max: u64,
        ) -> impl std::future::Future<Output = Result<FetchedFile, SearchError>> {
            std::future::ready(Err(download_refused(format!(
                "`{url}`: the cache was expected to answer"
            ))))
        }
    }

    /// A transport that writes its bytes and then fails, as a cut connection
    /// does.
    struct StagingTransport;

    impl FileTransport for StagingTransport {
        async fn fetch(
            &self,
            url: &str,
            destination: &Path,
            _bytes_max: u64,
        ) -> Result<FetchedFile, SearchError> {
            tokio::fs::write(destination, b"partial")
                .await
                .map_err(download_failure)?;
            Err(download_refused(format!("`{url}`: the connection ended")))
        }
    }

    /// A transport that records the URL it was asked for and then fails.
    #[derive(Default)]
    struct RecordingTransport {
        urls: Mutex<Vec<String>>,
    }

    impl RecordingTransport {
        fn recorded(&self) -> Vec<String> {
            self.urls
                .lock()
                .map(|urls| urls.clone())
                .unwrap_or_default()
        }
    }

    impl FileTransport for RecordingTransport {
        fn fetch(
            &self,
            url: &str,
            _destination: &Path,
            _bytes_max: u64,
        ) -> impl std::future::Future<Output = Result<FetchedFile, SearchError>> {
            if let Ok(mut urls) = self.urls.lock() {
                urls.push(url.to_owned());
            }
            std::future::ready(Err(download_refused(format!("`{url}`: recorded"))))
        }
    }

    /// Writes a full snapshot and the revision file naming its commit.
    fn write_snapshot(root: &Path, revision: &str, commit: &str) -> Fallible<std::path::PathBuf> {
        let cache = RepositoryCache::new(root, REPOSITORY);
        let snapshot = cache.snapshot(commit);
        std::fs::create_dir_all(&snapshot)?;
        for file in MODEL_FILES {
            std::fs::write(snapshot.join(file.name), file.name.as_bytes())?;
        }
        std::fs::create_dir_all(cache.refs())?;
        std::fs::write(cache.refs().join(revision), commit.as_bytes())?;
        Ok(snapshot)
    }

    fn origin(revision: &str) -> RepositoryOrigin {
        RepositoryOrigin::new(HUB_ENDPOINT, REPOSITORY, revision)
    }

    #[test]
    fn the_cache_root_is_the_hub_cache_variable_when_it_is_set() -> TestResult {
        let resolved = environment(&[(HUB_CACHE_VARIABLE, "/cache/hub")]).cache_root()?;
        assert_eq!(resolved, Path::new("/cache/hub"));
        Ok(())
    }

    #[test]
    fn the_cache_root_is_the_hub_below_the_hugging_face_home() -> TestResult {
        let resolved = environment(&[(HUB_HOME_VARIABLE, "/faces")]).cache_root()?;
        assert_eq!(resolved, Path::new("/faces/hub"));
        Ok(())
    }

    #[test]
    fn the_cache_root_is_the_hub_below_the_users_cache_home() -> TestResult {
        let resolved = environment(&[(CACHE_HOME_VARIABLE, "/caches")]).cache_root()?;
        assert_eq!(resolved, Path::new("/caches/huggingface/hub"));
        Ok(())
    }

    #[test]
    fn the_cache_root_is_the_hub_below_the_home_directory() -> TestResult {
        let resolved = environment(&[(USER_HOME_VARIABLE, "/people/ada")]).cache_root()?;
        assert_eq!(resolved, Path::new("/people/ada/.cache/huggingface/hub"));
        let profile = environment(&[(USER_PROFILE_VARIABLE, "/people/ada")]).cache_root()?;
        assert_eq!(profile, resolved, "Windows names the home directory too");
        Ok(())
    }

    #[test]
    fn an_empty_variable_counts_as_unset() -> TestResult {
        let resolved = environment(&[
            (HUB_CACHE_VARIABLE, ""),
            (HUB_HOME_VARIABLE, ""),
            (CACHE_HOME_VARIABLE, ""),
            (ENDPOINT_VARIABLE, ""),
            (USER_HOME_VARIABLE, "/people/ada"),
        ])
        .cache_root()?;
        assert_eq!(resolved, Path::new("/people/ada/.cache/huggingface/hub"));
        assert_eq!(
            environment(&[(ENDPOINT_VARIABLE, "")]).endpoint(),
            HUB_ENDPOINT
        );
        Ok(())
    }

    #[test]
    fn the_hub_cache_variable_wins_over_every_other() -> TestResult {
        let every = [
            (HUB_CACHE_VARIABLE, "/first"),
            (HUB_HOME_VARIABLE, "/second"),
            (CACHE_HOME_VARIABLE, "/third"),
            (USER_HOME_VARIABLE, "/fourth"),
        ];
        assert_eq!(environment(&every).cache_root()?, Path::new("/first"));
        assert_eq!(
            environment(&every[1..]).cache_root()?,
            Path::new("/second/hub"),
            "the Hugging Face home wins over the cache home"
        );
        assert_eq!(
            environment(&every[2..]).cache_root()?,
            Path::new("/third/huggingface/hub"),
            "the cache home wins over the home directory"
        );
        Ok(())
    }

    #[test]
    fn a_machine_with_no_home_directory_has_no_cache_root() {
        let error = environment(&[])
            .cache_root()
            .expect_err("no variable names a root");
        assert_eq!(
            error.fault().violation(),
            SearchViolation::ModelCacheUnavailable
        );
        let rendered = error.to_string();
        assert!(rendered.contains("model_cache_unavailable"), "{rendered}");
        assert!(rendered.contains(HUB_CACHE_VARIABLE), "{rendered}");
        assert!(rendered.contains(USER_HOME_VARIABLE), "{rendered}");
    }

    #[test]
    fn the_process_environment_is_read_by_the_names_every_hugging_face_client_uses() {
        let read = HubEnvironment::from_process();
        let expected = environment(
            &[
                HUB_CACHE_VARIABLE,
                HUB_HOME_VARIABLE,
                CACHE_HOME_VARIABLE,
                USER_HOME_VARIABLE,
                USER_PROFILE_VARIABLE,
                ENDPOINT_VARIABLE,
            ]
            .into_iter()
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .filter(|value| !value.is_empty())
                    .map(|value| (name, value))
            })
            .collect::<Vec<_>>()
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect::<Vec<_>>(),
        );
        assert_eq!(read, expected);
    }

    #[test]
    fn a_repository_identifier_becomes_one_directory_below_the_hub_root() {
        assert_eq!(repository_directory(REPOSITORY), REPOSITORY_DIRECTORY);
        assert_eq!(repository_directory("plain"), "models--plain");
    }

    #[tokio::test]
    async fn a_cache_that_already_holds_every_file_is_read_without_a_fetch() -> TestResult {
        let root = tempfile::tempdir()?;
        let snapshot = write_snapshot(root.path(), DEFAULT_REVISION, COMMIT)?;
        let files = acquire_repository(
            &origin(DEFAULT_REVISION),
            limits(1),
            &RefusingTransport,
            root.path(),
        )
        .await?;
        assert_eq!(files, ModelFiles::in_directory(&snapshot)?);
        let rendered = format!("{files:?}");
        assert!(
            rendered.contains(&snapshot.display().to_string()),
            "the returned paths point into the snapshot: {rendered}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_snapshot_entry_that_is_a_symlink_to_its_blob_is_a_hit() -> TestResult {
        let root = tempfile::tempdir()?;
        let cache = RepositoryCache::new(root.path(), REPOSITORY);
        let snapshot = cache.snapshot(COMMIT);
        std::fs::create_dir_all(&snapshot)?;
        std::fs::create_dir_all(cache.blobs())?;
        for file in MODEL_FILES {
            let blob = cache.blobs().join(file.name);
            std::fs::write(&blob, file.name.as_bytes())?;
            std::os::unix::fs::symlink(&blob, snapshot.join(file.name))?;
        }
        std::fs::create_dir_all(cache.refs())?;
        std::fs::write(cache.refs().join(DEFAULT_REVISION), COMMIT.as_bytes())?;
        let files = acquire_repository(
            &origin(DEFAULT_REVISION),
            limits(1),
            &RefusingTransport,
            root.path(),
        )
        .await?;
        assert_eq!(files, ModelFiles::in_directory(&snapshot)?);
        Ok(())
    }

    #[tokio::test]
    async fn a_snapshot_short_of_one_file_is_no_hit() -> TestResult {
        let root = tempfile::tempdir()?;
        let snapshot = write_snapshot(root.path(), DEFAULT_REVISION, COMMIT)?;
        std::fs::remove_file(snapshot.join(CONFIGURATION_FILE))?;
        let cache = RepositoryCache::new(root.path(), REPOSITORY);
        assert!(cache.cached_snapshot(DEFAULT_REVISION).is_none());
        Ok(())
    }

    #[test]
    fn a_revision_file_addressing_a_path_outside_the_repository_is_no_hit() -> TestResult {
        let root = tempfile::tempdir()?;
        let cache = RepositoryCache::new(root.path(), REPOSITORY);
        std::fs::create_dir_all(cache.refs())?;
        std::fs::write(cache.refs().join(DEFAULT_REVISION), "../elsewhere")?;
        assert!(cache.cached_snapshot(DEFAULT_REVISION).is_none());
        assert!(
            cache.cached_snapshot("absent").is_none(),
            "a revision the cache never recorded is no hit"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_miss_writes_the_blob_the_snapshot_and_the_revision() -> TestResult {
        let root = tempfile::tempdir()?;
        let transport = FixtureTransport::new(Some(COMMIT), Some(ETAG));
        acquire_repository(
            &origin(DEFAULT_REVISION),
            limits(1),
            &transport,
            root.path(),
        )
        .await?;
        let cache = RepositoryCache::new(root.path(), REPOSITORY);
        assert!(
            cache.blobs().join(ETAG).is_file(),
            "the blob holds the bytes"
        );
        let snapshot = cache.snapshot(COMMIT);
        for file in MODEL_FILES {
            assert!(
                snapshot.join(file.name).exists(),
                "{} must be addressable in the snapshot",
                file.name
            );
        }
        assert_eq!(
            std::fs::read_to_string(cache.refs().join(DEFAULT_REVISION))?,
            COMMIT
        );
        assert_eq!(transport.calls(), 3, "one fetch per model file");
        Ok(())
    }

    #[tokio::test]
    async fn a_response_without_a_commit_header_falls_back_to_the_revision() -> TestResult {
        let root = tempfile::tempdir()?;
        let transport = FixtureTransport::new(None, Some(ETAG));
        acquire_repository(
            &origin(DEFAULT_REVISION),
            limits(1),
            &transport,
            root.path(),
        )
        .await?;
        let cache = RepositoryCache::new(root.path(), REPOSITORY);
        assert!(cache.snapshot(DEFAULT_REVISION).is_dir());
        assert_eq!(
            std::fs::read_to_string(cache.refs().join(DEFAULT_REVISION))?,
            DEFAULT_REVISION
        );
        Ok(())
    }

    #[test]
    fn a_quoted_etag_and_a_weak_etag_are_both_reduced_to_the_bare_value() -> TestResult {
        assert_eq!(bare_etag("\"abc\""), "abc");
        assert_eq!(bare_etag("W/\"abc\""), "abc");
        assert_eq!(bare_etag("abc"), "abc");
        for declared in ["\"d41d8cd98f00b204\"", "W/\"d41d8cd98f00b204\"", ETAG] {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::ETAG, declared.parse()?);
            headers.insert(COMMIT_HEADER, COMMIT.parse()?);
            let fetched = FetchedFile::from_headers(&headers);
            assert_eq!(fetched.etag(), Some(ETAG), "{declared} must reduce");
            assert_eq!(fetched.commit(), Some(COMMIT));
        }
        let bare = FetchedFile::from_headers(&reqwest::header::HeaderMap::new());
        assert_eq!(bare, FetchedFile::default(), "no header declares nothing");
        Ok(())
    }

    #[tokio::test]
    async fn the_etag_the_origin_declared_names_the_blob() -> TestResult {
        let root = tempfile::tempdir()?;
        let transport = FixtureTransport::new(Some(COMMIT), Some(ETAG));
        acquire_repository(
            &origin(DEFAULT_REVISION),
            limits(1),
            &transport,
            root.path(),
        )
        .await?;
        let cache = RepositoryCache::new(root.path(), REPOSITORY);
        assert!(cache.blobs().join(ETAG).is_file());
        Ok(())
    }

    #[tokio::test]
    async fn an_origin_supplied_name_addressing_a_path_outside_the_repository_is_refused()
    -> TestResult {
        for (commit, etag) in [(COMMIT, "../outside"), ("..", ETAG), (COMMIT, "")] {
            let root = tempfile::tempdir()?;
            let transport = FixtureTransport::new(Some(commit), Some(etag));
            let error = acquire_repository(
                &origin(DEFAULT_REVISION),
                limits(1),
                &transport,
                root.path(),
            )
            .await
            .expect_err("an origin cannot name a path outside its repository");
            assert_eq!(
                error.fault().violation(),
                SearchViolation::ModelDownloadFailed
            );
            let entries: Vec<_> = std::fs::read_dir(root.path())?.collect();
            assert_eq!(entries.len(), 1, "only the repository directory was made");
            assert!(root.path().join(REPOSITORY_DIRECTORY).is_dir());
            assert!(!root.path().join("outside").exists());
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_response_without_an_etag_is_refused() -> TestResult {
        let root = tempfile::tempdir()?;
        let transport = FixtureTransport::new(Some(COMMIT), None);
        let error = acquire_repository(
            &origin(DEFAULT_REVISION),
            limits(1),
            &transport,
            root.path(),
        )
        .await
        .expect_err("a blob has no name without an etag");
        assert_eq!(
            error.fault().violation(),
            SearchViolation::ModelDownloadFailed
        );
        assert!(error.to_string().contains("etag"), "{error}");
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_fetch_leaves_nothing_a_later_run_reads() -> TestResult {
        let root = tempfile::tempdir()?;
        let error = acquire_repository(
            &origin(DEFAULT_REVISION),
            limits(1),
            &StagingTransport,
            root.path(),
        )
        .await
        .expect_err("a cut connection must refuse");
        assert_eq!(
            error.fault().violation(),
            SearchViolation::ModelDownloadFailed
        );
        let cache = RepositoryCache::new(root.path(), REPOSITORY);
        assert!(!cache.refs().join(DEFAULT_REVISION).exists());
        assert_eq!(
            std::fs::read_dir(cache.blobs())?.count(),
            0,
            "the staged bytes are removed"
        );
        assert!(cache.cached_snapshot(DEFAULT_REVISION).is_none());
        Ok(())
    }

    #[tokio::test]
    async fn a_fetch_that_fails_once_and_then_succeeds_acquires() -> TestResult {
        let root = tempfile::tempdir()?;
        let transport = FixtureTransport::failing(1);
        acquire_repository(
            &origin(DEFAULT_REVISION),
            limits(2),
            &transport,
            root.path(),
        )
        .await?;
        assert_eq!(transport.calls(), 4, "one file cost two fetches");
        Ok(())
    }

    #[tokio::test]
    async fn a_fetch_that_always_fails_stops_at_the_attempt_bound_with_the_last_failure()
    -> TestResult {
        for attempts in [1_u32, 3] {
            let root = tempfile::tempdir()?;
            let transport = FixtureTransport::failing(u32::MAX);
            let error = acquire_repository(
                &origin(DEFAULT_REVISION),
                limits(attempts),
                &transport,
                root.path(),
            )
            .await
            .expect_err("every attempt failed");
            assert_eq!(transport.calls(), attempts, "the attempt bound is exact");
            assert!(
                error
                    .to_string()
                    .contains(&format!("fixture failure {attempts}")),
                "the last failure is returned, not a synthetic one: {error}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn the_endpoint_variable_changes_the_url_the_transport_is_asked_for() -> TestResult {
        let root = tempfile::tempdir()?;
        let named = environment(&[(ENDPOINT_VARIABLE, "http://127.0.0.1:9/mirror/")]);
        let mirrored = RepositoryOrigin::new(named.endpoint(), REPOSITORY, DEFAULT_REVISION);
        let transport = RecordingTransport::default();
        let refused = acquire_repository(&mirrored, limits(1), &transport, root.path()).await;
        assert!(refused.is_err(), "the recording transport never answers");
        assert_eq!(
            transport.recorded(),
            ["http://127.0.0.1:9/mirror/BAAI/bge-small-en-v1.5/resolve/main/config.json"]
        );
        assert_eq!(
            origin("abc").file_url("model.safetensors"),
            "https://huggingface.co/BAAI/bge-small-en-v1.5/resolve/abc/model.safetensors"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_directory_source_touches_neither_the_network_nor_the_cache() -> TestResult {
        let root = tempfile::tempdir()?;
        for file in MODEL_FILES {
            std::fs::write(root.path().join(file.name), file.name.as_bytes())?;
        }
        let source = ModelSource::Directory(root.path().to_path_buf());
        let files = acquire_from(&source, limits(1), &environment(&[])).await?;
        assert_eq!(files, ModelFiles::in_directory(root.path())?);
        Ok(())
    }

    #[tokio::test]
    async fn a_repository_source_reads_the_cache_root_the_environment_names() -> TestResult {
        let root = tempfile::tempdir()?;
        let snapshot = write_snapshot(root.path(), DEFAULT_REVISION, COMMIT)?;
        let named = environment(&[(HUB_CACHE_VARIABLE, &root.path().display().to_string())]);
        let source = ModelSource::repository(REPOSITORY)?;
        let files = acquire_from(&source, limits(1), &named).await?;
        assert_eq!(files, ModelFiles::in_directory(&snapshot)?);
        let nowhere = acquire_from(&source, limits(1), &environment(&[]))
            .await
            .expect_err("no variable names a cache root");
        assert_eq!(
            nowhere.fault().violation(),
            SearchViolation::ModelCacheUnavailable
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_blob_already_in_the_cache_is_kept() -> TestResult {
        let root = tempfile::tempdir()?;
        let blob = root.path().join(ETAG);
        std::fs::write(&blob, b"original")?;
        let staged = root.path().join("staged");
        std::fs::write(&staged, b"replacement")?;
        place_blob(&staged, &blob).await?;
        assert_eq!(std::fs::read_to_string(&blob)?, "original");
        assert!(!staged.exists(), "the staged bytes are removed");
        Ok(())
    }

    #[tokio::test]
    async fn a_blob_that_cannot_be_linked_is_copied_into_the_snapshot() -> TestResult {
        let root = tempfile::tempdir()?;
        let snapshot = root.path().join("snapshot");
        std::fs::create_dir_all(&snapshot)?;
        let blob = root.path().join(ETAG);
        std::fs::write(&blob, b"weights")?;
        copy_atomically(&blob, &snapshot, CONFIGURATION_FILE).await?;
        assert_eq!(
            std::fs::read_to_string(snapshot.join(CONFIGURATION_FILE))?,
            "weights"
        );
        let absent = root.path().join("absent");
        let error = place_in_snapshot(&absent, &snapshot, "tokenizer.json")
            .await
            .expect_err("a blob that is not there can neither be linked nor copied");
        assert_eq!(
            error.fault().violation(),
            SearchViolation::ModelDownloadFailed
        );
        place_in_snapshot(&blob, &snapshot, CONFIGURATION_FILE).await?;
        assert_eq!(
            std::fs::read_to_string(snapshot.join(CONFIGURATION_FILE))?,
            "weights",
            "a name already in the snapshot is kept"
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_write_that_cannot_be_renamed_into_place_removes_what_it_staged() -> TestResult {
        let root = tempfile::tempdir()?;
        let directory = root.path().join(REFS_DIRECTORY);
        std::fs::create_dir_all(directory.join(DEFAULT_REVISION).join("occupied"))?;
        let error = write_atomically(&directory, DEFAULT_REVISION, COMMIT.as_bytes())
            .await
            .expect_err("a non-empty directory cannot be replaced by a rename");
        assert_eq!(
            error.fault().violation(),
            SearchViolation::ModelDownloadFailed
        );
        assert_eq!(
            std::fs::read_dir(&directory)?.count(),
            1,
            "the staged file was removed"
        );
        Ok(())
    }

    /// Serves `responses` in order on a loopback port, one connection each.
    fn canned_origin(responses: Vec<Vec<u8>>) -> Fallible<(String, std::thread::JoinHandle<()>)> {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request);
                let _ = stream.write_all(&response);
            }
        });
        Ok((format!("http://{address}"), server))
    }

    async fn assert_refused(
        transport: &HubTransport,
        url: &str,
        destination: &Path,
        bytes_max: u64,
        expected: SearchViolation,
    ) {
        let error = FileTransport::fetch(transport, url, destination, bytes_max)
            .await
            .expect_err("the fetch must be refused");
        assert_eq!(error.fault().violation(), expected, "{url}: {error}");
    }

    #[tokio::test]
    async fn the_live_transport_reads_headers_and_enforces_status_and_bounds() -> TestResult {
        let responses = vec![
            b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\netag: \"d41d8cd98f00b204\"\r\nx-repo-commit: 0e0ac2cbe0ee1c1d\r\nconnection: close\r\n\r\npayload".to_vec(),
            b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: close\r\n\r\npayload".to_vec(),
            b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\npayload".to_vec(),
            b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: close\r\n\r\nshort".to_vec(),
            b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:9/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
        ];
        let (endpoint, server) = canned_origin(responses)?;
        let transport = HubTransport::new(Duration::from_secs(5))?;
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("payload");

        let fetched =
            FileTransport::fetch(&transport, &format!("{endpoint}/ok"), &destination, 64).await?;
        assert_eq!(fetched.commit(), Some(COMMIT));
        assert_eq!(fetched.etag(), Some(ETAG));
        assert_eq!(std::fs::read(&destination)?, b"payload");

        let too_large = SearchViolation::ModelDownloadTooLarge;
        let failed = SearchViolation::ModelDownloadFailed;
        assert_refused(
            &transport,
            &format!("{endpoint}/declared"),
            &destination,
            3,
            too_large,
        )
        .await;
        assert_refused(
            &transport,
            &format!("{endpoint}/unsized"),
            &destination,
            3,
            too_large,
        )
        .await;
        assert_refused(
            &transport,
            &format!("{endpoint}/truncated"),
            &destination,
            256,
            failed,
        )
        .await;
        assert_refused(
            &transport,
            &format!("{endpoint}/absent"),
            &destination,
            64,
            failed,
        )
        .await;
        assert_refused(
            &transport,
            &format!("{endpoint}/redirect"),
            &destination,
            64,
            too_large,
        )
        .await;

        server.join().map_err(|_| "the canned origin must finish")?;
        Ok(())
    }

    #[tokio::test]
    async fn the_live_transport_follows_a_secure_redirect_within_its_hop_bound() -> TestResult {
        let responses = vec![
            b"HTTP/1.1 302 Found\r\nlocation: https://127.0.0.1:9/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
        ];
        let (endpoint, server) = canned_origin(responses)?;
        let transport = HubTransport::new(Duration::from_secs(5))?;
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("payload");
        let url = format!("{endpoint}/redirect");
        let error = FileTransport::fetch(&transport, &url, &destination, 64)
            .await
            .expect_err("the hop is followed to a port that refuses it");
        assert_eq!(
            error.fault().violation(),
            SearchViolation::ModelDownloadFailed,
            "an https hop is followed rather than stopped: {error}"
        );
        server.join().map_err(|_| "the canned origin must finish")?;
        Ok(())
    }
}
