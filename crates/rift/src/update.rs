//! Bounded, checksummed self-update for official Rift releases.

use rift_core::constants::{
    CHECKSUM_ENTRY_COUNT_MAX, CHECKSUM_MANIFEST_BYTES_MAX, HTTPS_SCHEME, REDIRECT_HOPS_MAX,
    RELEASE_API_URL, RELEASE_ARCHIVE_BYTES_MAX, RELEASE_ARCHIVE_MEMBER_COUNT,
    RELEASE_BINARY_BYTES_MAX, RELEASE_DOCUMENT_BYTES_MAX, RELEASE_DOWNLOAD_BASE_URL,
    RELEASE_DOWNLOAD_TIMEOUT, RELEASE_METADATA_BYTES_MAX, SHA256_HEX_LENGTH,
};
use rift_core::{CliCode, ErrorDescriptor, ErrorName};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::{Command, Stdio};
use tokio::io::AsyncWriteExt as _;

/// File name for the staged latest-release metadata response.
const RELEASE_METADATA_FILE_NAME: &str = "latest.json";
/// User agent identifying the Rift updater to GitHub.
const UPDATE_USER_AGENT: &str = "rift-updater";
/// Accept header value requesting GitHub's JSON release representation.
const GITHUB_JSON_ACCEPT: &str = "application/vnd.github+json";
/// Two-space separator of the `sha256sum` text-mode manifest format.
const SHA256SUM_SEPARATOR: &str = "  ";
/// Issue tracker for reporting update failures that persist across retries.
const ISSUE_TRACKER_URL: &str = "https://github.com/volarized/rift/issues";

/// Sibling file name staging the incoming Windows binary.
#[cfg(windows)]
const WINDOWS_UPDATE_PREPARED_NAME: &str = ".rift-update-new.exe";
/// Sibling file name holding the replaced Windows binary until cleanup.
#[cfg(windows)]
const WINDOWS_UPDATE_BACKUP_NAME: &str = ".rift-update-old.exe";
/// Hidden subcommand run on the new binary to delete the replaced one.
///
/// Must match the hidden `__CleanupUpdate` subcommand name in `main.rs`.
#[cfg(windows)]
const CLEANUP_SUBCOMMAND: &str = "__cleanup-update";
/// Maximum removal attempts for one replaced Windows binary.
#[cfg(windows)]
const CLEANUP_RETRY_COUNT_MAX: u32 = 40;
/// Delay between replaced-binary removal attempts.
#[cfg(windows)]
const CLEANUP_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const RELEASE_TARGET: &str = "aarch64-apple-darwin";
#[cfg(all(target_arch = "x86_64", target_os = "macos"))]
const RELEASE_TARGET: &str = "x86_64-apple-darwin";
#[cfg(all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"))]
const RELEASE_TARGET: &str = "aarch64-unknown-linux-gnu";
#[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"))]
const RELEASE_TARGET: &str = "x86_64-unknown-linux-gnu";
#[cfg(all(target_arch = "aarch64", target_os = "windows"))]
const RELEASE_TARGET: &str = "aarch64-pc-windows-msvc";
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
const RELEASE_TARGET: &str = "x86_64-pc-windows-msvc";

#[cfg(not(any(
    all(target_arch = "aarch64", target_os = "macos"),
    all(target_arch = "x86_64", target_os = "macos"),
    all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"),
    all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"),
    all(target_arch = "aarch64", target_os = "windows"),
    all(target_arch = "x86_64", target_os = "windows"),
)))]
compile_error!("rift update supports only published Rift release targets");

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum UpdateOutcome {
    Current(Version),
    Unreleased {
        current: Version,
        latest: Version,
    },
    Updated {
        from: Version,
        to: Version,
        cleanup: OldBinaryCleanup,
    },
}

/// What became of the binary an update replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum OldBinaryCleanup {
    /// Replacement was atomic; no old binary remains.
    Unnecessary,
    /// A detached process deletes the old binary once this one exits.
    #[cfg_attr(not(windows), allow(dead_code))]
    Scheduled,
    /// The old binary still sits at the contained path.
    #[cfg_attr(not(windows), allow(dead_code))]
    Remaining(PathBuf),
}

impl fmt::Display for UpdateOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Current(version) => write!(
                formatter,
                "You're already using the latest version of rift {version} 🎉"
            ),
            Self::Unreleased { current, latest } => write!(
                formatter,
                "You're running a rift version that has never been released. That's a spirit for innovation! (current v{current}, latest release v{latest})"
            ),
            Self::Updated { from, to, cleanup } => {
                write!(formatter, "Updated Rift from v{from} to v{to}.")?;
                match cleanup {
                    OldBinaryCleanup::Unnecessary => Ok(()),
                    OldBinaryCleanup::Scheduled => formatter.write_str(
                        " Rift will automatically clean up the old binary after the update.",
                    ),
                    OldBinaryCleanup::Remaining(path) => write!(
                        formatter,
                        " We were not able to clean up the old binary, please check {}.",
                        path.display()
                    ),
                }
            }
        }
    }
}

/// Opaque updater failure.
#[derive(Debug)]
pub(super) struct UpdateError {
    name: ErrorName,
    message: Cow<'static, str>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl UpdateError {
    fn new(name: ErrorName, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            name,
            message: message.into(),
            source: None,
        }
    }

    fn caused_by(
        name: ErrorName,
        message: impl Into<Cow<'static, str>>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            name,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    /// Returns canonical registry metadata for this failure.
    pub(super) fn descriptor(&self) -> ErrorDescriptor {
        self.name.descriptor()
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for UpdateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
}

#[derive(Debug)]
struct ReleaseArtifact {
    version: Version,
    archive_name: String,
    checksum_name: String,
    root: String,
}

impl ReleaseArtifact {
    fn new(version: Version) -> Self {
        let tag = format!("v{version}");
        let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
        let root = format!("rift-{tag}-{RELEASE_TARGET}");
        Self {
            version,
            archive_name: format!("{root}.{extension}"),
            checksum_name: format!("rift-{tag}-checksums.sha256"),
            root,
        }
    }

    fn binary_name() -> &'static str {
        if cfg!(windows) { "rift.exe" } else { "rift" }
    }

    fn binary_member(&self) -> String {
        format!("{}/{}", self.root, Self::binary_name())
    }

    fn members(&self) -> [String; RELEASE_ARCHIVE_MEMBER_COUNT] {
        [
            self.binary_member(),
            format!("{}/README.md", self.root),
            format!("{}/LICENSE.md", self.root),
        ]
    }

    fn download_url(&self, name: &str) -> String {
        format!("{RELEASE_DOWNLOAD_BASE_URL}/v{}/{name}", self.version)
    }
}

trait DownloadTransport {
    async fn download(
        &self,
        url: &str,
        destination: &Path,
        bytes_max: u64,
    ) -> Result<(), UpdateError>;
}

trait UpdateSource {
    async fn latest_version(&self, directory: &Path) -> Result<Version, UpdateError>;
    async fn stage(&self, directory: &Path, version: Version) -> Result<PathBuf, UpdateError>;
}

trait Publisher {
    async fn publish(
        &self,
        current: &Path,
        candidate: &Path,
    ) -> Result<OldBinaryCleanup, UpdateError>;
}

struct ReqwestTransport {
    client: reqwest::Client,
}
struct GitHubReleaseSource<T> {
    transport: T,
}
struct AtomicPublisher;

/// Replaces the current binary with the latest official release.
///
/// Release lookup and downloads run on the async HTTP client; checksum
/// verification, extraction, and replacement are synchronous stages on the
/// runtime's blocking pool.
///
/// # Errors
///
/// Returns an [`UpdateError`] carrying its registry code when the release
/// lookup, download, checksum verification, extraction, or replacement
/// fails.
///
/// # Cancel safety
///
/// Not cancel safe. Dropping the future mid-download stops the transfer and
/// removes the staging directory with every partial file. Dropping it while
/// a blocking stage runs detaches that stage, which completes on the
/// blocking pool with its outcome unreported: verification and extraction
/// then fail benignly against the removed staging directory, a Unix
/// replacement publishes atomically or cleans its staged sibling, and a
/// Windows replacement may leave sibling files that the next `rift update`
/// run removes or reports.
pub(super) async fn update() -> Result<UpdateOutcome, UpdateError> {
    let current = std::env::current_exe().map_err(locate_error)?;
    let current_version = Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|error| version_error(env!("CARGO_PKG_VERSION"), &current, error))?;
    update_with(
        &GitHubReleaseSource {
            transport: ReqwestTransport::new()?,
        },
        &AtomicPublisher,
        &current,
        current_version,
    )
    .await
}

async fn update_with(
    source: &impl UpdateSource,
    publisher: &impl Publisher,
    current_path: &Path,
    current_version: Version,
) -> Result<UpdateOutcome, UpdateError> {
    let staging = run_blocking(|| tempfile::tempdir().map_err(staging_error)).await?;
    let latest_version = source.latest_version(staging.path()).await?;
    match latest_version.cmp(&current_version) {
        Ordering::Equal => Ok(UpdateOutcome::Current(current_version)),
        Ordering::Less => Ok(UpdateOutcome::Unreleased {
            current: current_version,
            latest: latest_version,
        }),
        Ordering::Greater => {
            let candidate = source.stage(staging.path(), latest_version.clone()).await?;
            let cleanup = publisher.publish(current_path, &candidate).await?;
            Ok(UpdateOutcome::Updated {
                from: current_version,
                to: latest_version,
                cleanup,
            })
        }
    }
}

/// Runs one synchronous updater stage on the runtime's blocking pool.
///
/// A panic inside the stage resumes on the caller; the cancelled arm is
/// unreachable because the runtime lives until `update` returns.
async fn run_blocking<T: Send + 'static>(
    stage: impl FnOnce() -> Result<T, UpdateError> + Send + 'static,
) -> Result<T, UpdateError> {
    match tokio::task::spawn_blocking(stage).await {
        Ok(result) => result,
        Err(join_error) if join_error.is_panic() => {
            std::panic::resume_unwind(join_error.into_panic())
        }
        Err(join_error) => unreachable!(
            "the updater's blocking stage is never cancelled: the runtime lives until update \
             returns, join_error={join_error}"
        ),
    }
}

fn locate_error(error: io::Error) -> UpdateError {
    let invoked_as = std::env::args_os().next().map_or_else(
        || Cow::Borrowed("rift"),
        |argument| Cow::Owned(argument.to_string_lossy().into_owned()),
    );
    UpdateError::caused_by(
        ErrorName::Cli(CliCode::UpdateBinaryInvalid),
        format!(
            "current Rift executable (invoked as `{invoked_as}`) could not be located: {error}: reinstall Rift if the binary was moved or deleted"
        ),
        error,
    )
}

fn version_error(raw: &str, installed_at: &Path, error: semver::Error) -> UpdateError {
    UpdateError::caused_by(
        ErrorName::Cli(CliCode::UpdateBinaryInvalid),
        format!(
            "installed Rift version `{raw}` at `{}` is invalid: {error}: reinstall Rift from an official release",
            installed_at.display()
        ),
        error,
    )
}

fn staging_error(error: io::Error) -> UpdateError {
    let staging_root = std::env::temp_dir();
    let space = fs4::available_space(&staging_root).map_or_else(
        |_| Cow::Borrowed("free space unknown"),
        |bytes| Cow::Owned(format!("{bytes} bytes free")),
    );
    UpdateError::caused_by(
        ErrorName::Cli(CliCode::UpdateStagingFailed),
        format!(
            "update staging directory could not be created under `{}` ({space}): {error}: ensure the directory is writable and has free space, then retry `rift update`",
            staging_root.display()
        ),
        error,
    )
}

impl ReqwestTransport {
    fn new() -> Result<Self, UpdateError> {
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
            .timeout(RELEASE_DOWNLOAD_TIMEOUT)
            .min_tls_version(reqwest::tls::Version::TLS_1_2)
            .user_agent(UPDATE_USER_AGENT)
            .build()
            .map_err(download_error)?;
        Ok(Self { client })
    }
}

impl DownloadTransport for ReqwestTransport {
    async fn download(
        &self,
        url: &str,
        destination: &Path,
        bytes_max: u64,
    ) -> Result<(), UpdateError> {
        let mut response = self
            .client
            .get(url)
            .header(reqwest::header::ACCEPT, GITHUB_JSON_ACCEPT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(download_error)?;
        if response
            .content_length()
            .is_some_and(|length| length > bytes_max)
        {
            return Err(download_too_large(bytes_max));
        }
        let mut output = tokio::fs::File::create(destination)
            .await
            .map_err(download_error)?;
        let received = write_bounded_body(&mut response, &mut output, bytes_max).await?;
        if received == 0 || received > bytes_max {
            return Err(download_too_large(bytes_max));
        }
        output.sync_all().await.map_err(download_error)?;
        Ok(())
    }
}

/// Streams a response body to `output`, counting at most one byte past `bytes_max`.
///
/// Reading stops at the sentinel ceiling, so an oversized body is detected
/// without being drained or buffered whole.
async fn write_bounded_body(
    response: &mut reqwest::Response,
    output: &mut tokio::fs::File,
    bytes_max: u64,
) -> Result<u64, UpdateError> {
    let ceiling = bytes_max_with_sentinel(bytes_max);
    let mut received = 0_u64;
    while received < ceiling {
        let Some(chunk) = response.chunk().await.map_err(download_error)? else {
            break;
        };
        let kept = bytes_within_budget(chunk.len(), ceiling - received);
        output
            .write_all(&chunk[..kept])
            .await
            .map_err(download_error)?;
        received = received.saturating_add(u64::try_from(kept).unwrap_or(u64::MAX));
    }
    Ok(received)
}

/// Bytes of one body chunk that fit within the remaining counted budget.
fn bytes_within_budget(chunk_length: usize, budget: u64) -> usize {
    usize::try_from(budget).map_or(chunk_length, |budget| chunk_length.min(budget))
}

impl<T: DownloadTransport> UpdateSource for GitHubReleaseSource<T> {
    async fn latest_version(&self, directory: &Path) -> Result<Version, UpdateError> {
        let metadata_path = directory.join(RELEASE_METADATA_FILE_NAME);
        self.transport
            .download(RELEASE_API_URL, &metadata_path, RELEASE_METADATA_BYTES_MAX)
            .await?;
        let bytes = tokio::fs::read(&metadata_path)
            .await
            .map_err(release_error)?;
        parse_release_metadata(&bytes)
    }

    async fn stage(&self, directory: &Path, version: Version) -> Result<PathBuf, UpdateError> {
        let artifact = ReleaseArtifact::new(version);
        let manifest_path = directory.join(&artifact.checksum_name);
        let archive_path = directory.join(&artifact.archive_name);
        self.transport
            .download(
                &artifact.download_url(&artifact.checksum_name),
                &manifest_path,
                CHECKSUM_MANIFEST_BYTES_MAX,
            )
            .await?;
        self.transport
            .download(
                &artifact.download_url(&artifact.archive_name),
                &archive_path,
                RELEASE_ARCHIVE_BYTES_MAX,
            )
            .await?;
        let directory = directory.to_owned();
        run_blocking(move || {
            stage_verified_candidate(&archive_path, &manifest_path, &directory, &artifact)
        })
        .await
    }
}

/// Parses GitHub's latest-release document into a stable release version.
fn parse_release_metadata(bytes: &[u8]) -> Result<Version, UpdateError> {
    let release: LatestRelease = serde_json::from_slice(bytes).map_err(release_error)?;
    parse_release_tag(&release.tag_name)
}

/// Verifies the archive checksum, then extracts and validates the binary candidate.
fn stage_verified_candidate(
    archive_path: &Path,
    manifest_path: &Path,
    directory: &Path,
    artifact: &ReleaseArtifact,
) -> Result<PathBuf, UpdateError> {
    verify_checksum(archive_path, manifest_path, &artifact.archive_name)?;
    extract_candidate(archive_path, directory, artifact)
}

fn parse_release_tag(tag: &str) -> Result<Version, UpdateError> {
    let value = tag.strip_prefix('v').ok_or_else(|| {
        UpdateError::new(
            ErrorName::Cli(CliCode::UpdateReleaseInvalid),
            release_tag_invalid(tag),
        )
    })?;
    let version = Version::parse(value).map_err(|error| {
        UpdateError::caused_by(
            ErrorName::Cli(CliCode::UpdateReleaseInvalid),
            release_tag_invalid(tag),
            error,
        )
    })?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err(UpdateError::new(
            ErrorName::Cli(CliCode::UpdateReleaseInvalid),
            format!(
                "release tag `{tag}` is a pre-release or build: only stable releases of the form `vMAJOR.MINOR.PATCH` are supported"
            ),
        ));
    }
    Ok(version)
}

fn release_tag_invalid(tag: &str) -> String {
    format!(
        "release tag `{tag}` is invalid: expected the form `vMAJOR.MINOR.PATCH`, such as `v0.0.2`"
    )
}

#[cfg(test)]
pub(super) fn error_for_test() -> UpdateError {
    parse_release_tag("vinvalid").expect_err("fixture tag must be invalid")
}

fn require_bounded_file(name: ErrorName, path: &Path, bytes_max: u64) -> Result<u64, UpdateError> {
    let metadata = fs::metadata(path).map_err(|error| {
        UpdateError::caused_by(
            name,
            format!(
                "downloaded release file at `{}` could not be inspected: {error}: retry `rift update`",
                path.display()
            ),
            error,
        )
    })?;
    if !metadata.is_file() {
        return Err(UpdateError::new(
            name,
            format!(
                "downloaded release file at `{}` is not a regular file: retry `rift update` or create an issue at {ISSUE_TRACKER_URL}",
                path.display()
            ),
        ));
    }
    let length = metadata.len();
    if length == 0 || length > bytes_max {
        return Err(UpdateError::new(
            name,
            format!(
                "downloaded release file at `{}` has incorrect size of {length} bytes, expected between 1 and {bytes_max} bytes: retry `rift update` or create an issue at {ISSUE_TRACKER_URL}",
                path.display()
            ),
        ));
    }
    Ok(length)
}

fn verify_checksum(archive: &Path, manifest: &Path, archive_name: &str) -> Result<(), UpdateError> {
    require_bounded_file(
        ErrorName::Cli(CliCode::UpdateReleaseInvalid),
        manifest,
        CHECKSUM_MANIFEST_BYTES_MAX,
    )?;
    let content = fs::read_to_string(manifest).map_err(checksum_error)?;
    let mut expected = None;
    for (index, line) in content.lines().enumerate() {
        if index >= CHECKSUM_ENTRY_COUNT_MAX {
            return Err(checksum_invalid());
        }
        let (digest, name) = line
            .split_once(SHA256SUM_SEPARATOR)
            .ok_or_else(checksum_invalid)?;
        if digest.len() != SHA256_HEX_LENGTH || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(checksum_invalid());
        }
        if name == archive_name && expected.replace(digest).is_some() {
            return Err(checksum_invalid());
        }
    }
    let expected = expected.ok_or_else(checksum_invalid)?;
    let actual = sha256(archive)?;
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(UpdateError::new(
            ErrorName::Cli(CliCode::UpdateChecksumMismatch),
            format!(
                "the downloaded release does not match its published checksum: expected {expected}, actual {actual}; retry `rift update`, and raise an issue at {ISSUE_TRACKER_URL} if the mismatch repeats"
            ),
        ));
    }
    Ok(())
}

fn sha256(path: &Path) -> Result<String, UpdateError> {
    require_bounded_file(
        ErrorName::Cli(CliCode::UpdateArchiveInvalid),
        path,
        RELEASE_ARCHIVE_BYTES_MAX,
    )?;
    let mut file = fs::File::open(path).map_err(checksum_error)?;
    let mut digest = Sha256::new();
    io::copy(&mut file, &mut digest).map_err(checksum_error)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn checksum_invalid() -> UpdateError {
    UpdateError::new(
        ErrorName::Cli(CliCode::UpdateChecksumMismatch),
        "release checksum manifest is invalid: expected `sha256sum`-format lines naming the release archive exactly once; retry `rift update`",
    )
}

fn checksum_error(error: io::Error) -> UpdateError {
    UpdateError::caused_by(
        ErrorName::Cli(CliCode::UpdateChecksumMismatch),
        "release checksum could not be verified: retry `rift update`",
        error,
    )
}

fn release_error(error: impl std::error::Error + Send + Sync + 'static) -> UpdateError {
    UpdateError::caused_by(
        ErrorName::Cli(CliCode::UpdateReleaseInvalid),
        "latest release metadata is invalid: retry `rift update` or check https://github.com/volarized/rift/releases",
        error,
    )
}

fn download_error(error: impl std::error::Error + Send + Sync + 'static) -> UpdateError {
    UpdateError::caused_by(
        ErrorName::Cli(CliCode::UpdateDownloadFailed),
        "release download failed: check network access to github.com and retry `rift update`",
        error,
    )
}

fn download_too_large(bytes_max: u64) -> UpdateError {
    UpdateError::new(
        ErrorName::Cli(CliCode::UpdateDownloadFailed),
        format!(
            "release download was empty or exceeded {bytes_max} bytes: retry `rift update`; if this persists the release assets may be malformed"
        ),
    )
}

/// Read limit accepting one sentinel byte past `bytes_max` to detect oversized payloads.
const fn bytes_max_with_sentinel(bytes_max: u64) -> u64 {
    bytes_max.saturating_add(1)
}

fn copy_bounded(
    source: &mut impl Read,
    destination: &mut impl Write,
    bytes_max: u64,
) -> Result<(), UpdateError> {
    let copied = io::copy(
        &mut source.take(bytes_max_with_sentinel(bytes_max)),
        destination,
    )
    .map_err(archive_error)?;
    if copied == 0 || copied > bytes_max {
        return Err(UpdateError::new(
            ErrorName::Cli(CliCode::UpdateArchiveInvalid),
            format!(
                "release archive member is empty or exceeds {bytes_max} bytes: retry `rift update`; if this persists the release may be malformed"
            ),
        ));
    }
    Ok(())
}

fn extract_member(
    entry: &mut impl Read,
    path: &Path,
    size: u64,
    expected: &[String; RELEASE_ARCHIVE_MEMBER_COUNT],
    seen: &mut [bool; RELEASE_ARCHIVE_MEMBER_COUNT],
    candidate: &Path,
) -> Result<(), UpdateError> {
    let position = expected
        .iter()
        .position(|expected_path| path == Path::new(expected_path))
        .ok_or_else(archive_invalid)?;
    if seen[position] {
        return Err(archive_invalid());
    }
    seen[position] = true;
    let bytes_max = member_bytes_max(position);
    if size == 0 || size > bytes_max {
        return Err(archive_invalid());
    }
    if position == 0 {
        let mut output = fs::File::create(candidate).map_err(archive_error)?;
        copy_bounded(entry, &mut output, bytes_max)?;
        output.sync_all().map_err(archive_error)?;
    }
    Ok(())
}

fn finish_extraction(
    seen: [bool; RELEASE_ARCHIVE_MEMBER_COUNT],
    candidate: PathBuf,
) -> Result<PathBuf, UpdateError> {
    if seen != [true; RELEASE_ARCHIVE_MEMBER_COUNT] {
        return Err(archive_invalid());
    }
    validate_candidate(&candidate)?;
    Ok(candidate)
}

#[cfg(unix)]
fn extract_candidate(
    archive_path: &Path,
    directory: &Path,
    artifact: &ReleaseArtifact,
) -> Result<PathBuf, UpdateError> {
    let raw = fs::File::open(archive_path).map_err(archive_error)?;
    let decoder = flate2::read::GzDecoder::new(raw);
    let mut archive = tar::Archive::new(decoder);
    let expected = artifact.members();
    let mut seen = [false; RELEASE_ARCHIVE_MEMBER_COUNT];
    let candidate = directory.join(ReleaseArtifact::binary_name());
    let entries = archive.entries().map_err(archive_error)?;
    for (index, entry) in entries.enumerate() {
        if index >= RELEASE_ARCHIVE_MEMBER_COUNT {
            return Err(archive_invalid());
        }
        let mut entry = entry.map_err(archive_error)?;
        if !entry.header().entry_type().is_file() {
            return Err(archive_invalid());
        }
        let path = entry.path().map_err(archive_error)?.into_owned();
        let size = entry.size();
        extract_member(&mut entry, &path, size, &expected, &mut seen, &candidate)?;
    }
    finish_extraction(seen, candidate)
}

#[cfg(windows)]
fn extract_candidate(
    archive_path: &Path,
    directory: &Path,
    artifact: &ReleaseArtifact,
) -> Result<PathBuf, UpdateError> {
    let raw = fs::File::open(archive_path).map_err(archive_error)?;
    let mut archive = zip::ZipArchive::new(raw).map_err(archive_error)?;
    if archive.len() != RELEASE_ARCHIVE_MEMBER_COUNT {
        return Err(archive_invalid());
    }
    let expected = artifact.members();
    let mut seen = [false; RELEASE_ARCHIVE_MEMBER_COUNT];
    let candidate = directory.join(ReleaseArtifact::binary_name());
    for index in 0..RELEASE_ARCHIVE_MEMBER_COUNT {
        let mut entry = archive.by_index(index).map_err(archive_error)?;
        if !entry.is_file() {
            return Err(archive_invalid());
        }
        let path = entry.enclosed_name().ok_or_else(archive_invalid)?;
        let size = entry.size();
        extract_member(&mut entry, &path, size, &expected, &mut seen, &candidate)?;
    }
    finish_extraction(seen, candidate)
}

fn validate_candidate(path: &Path) -> Result<(), UpdateError> {
    require_bounded_file(
        ErrorName::Cli(CliCode::UpdateArchiveInvalid),
        path,
        RELEASE_BINARY_BYTES_MAX,
    )?;
    Ok(())
}

const fn member_bytes_max(position: usize) -> u64 {
    if position == 0 {
        RELEASE_BINARY_BYTES_MAX
    } else {
        RELEASE_DOCUMENT_BYTES_MAX
    }
}

fn archive_invalid() -> UpdateError {
    UpdateError::new(
        ErrorName::Cli(CliCode::UpdateArchiveInvalid),
        "release archive contents are invalid: expected exactly one binary, README.md, and LICENSE.md member; retry `rift update`",
    )
}

fn archive_error(error: impl std::error::Error + Send + Sync + 'static) -> UpdateError {
    UpdateError::caused_by(
        ErrorName::Cli(CliCode::UpdateArchiveInvalid),
        "release archive could not be extracted: retry `rift update`; if this persists the download may be corrupted",
        error,
    )
}

impl Publisher for AtomicPublisher {
    async fn publish(
        &self,
        current: &Path,
        candidate: &Path,
    ) -> Result<OldBinaryCleanup, UpdateError> {
        let current = current.to_owned();
        let candidate = candidate.to_owned();
        run_blocking(move || publish_candidate(&current, &candidate)).await
    }
}

#[cfg(unix)]
fn publish_candidate(current: &Path, candidate: &Path) -> Result<OldBinaryCleanup, UpdateError> {
    let parent = current.parent().ok_or_else(|| no_parent_error(current))?;
    let mut prepared = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| publish_error("creating a staging file in", parent, error))?;
    let mut source = fs::File::open(candidate)
        .map_err(|error| publish_error("opening the downloaded binary", candidate, error))?;
    copy_bounded(
        &mut source,
        prepared.as_file_mut(),
        RELEASE_BINARY_BYTES_MAX,
    )
    .map_err(|error| {
        UpdateError::caused_by(
            ErrorName::Cli(CliCode::UpdatePublishFailed),
            format!(
                "Rift update could not be published: copying the downloaded binary into `{}` failed: ensure the directory is writable and has free space, then retry `rift update`",
                parent.display()
            ),
            error,
        )
    })?;
    let permissions = fs::metadata(current)
        .map_err(|error| publish_error("reading permissions of", current, error))?
        .permissions();
    prepared
        .as_file()
        .set_permissions(permissions)
        .map_err(|error| {
            publish_error("setting permissions on the staged binary in", parent, error)
        })?;
    prepared
        .as_file()
        .sync_all()
        .map_err(|error| publish_error("flushing the staged binary in", parent, error))?;
    prepared
        .persist(current)
        .map_err(|error| publish_error("replacing", current, error.error))?;
    Ok(OldBinaryCleanup::Unnecessary)
}

#[cfg(windows)]
fn publish_candidate(current: &Path, candidate: &Path) -> Result<OldBinaryCleanup, UpdateError> {
    let prepared = sibling(current, WINDOWS_UPDATE_PREPARED_NAME)?;
    let backup = sibling(current, WINDOWS_UPDATE_BACKUP_NAME)?;
    if backup.exists() {
        return Err(UpdateError::new(
            ErrorName::Cli(CliCode::UpdatePublishFailed),
            format!(
                "another Rift update is pending cleanup: retry after the previous Rift process exits, or delete `{}`",
                backup.display()
            ),
        ));
    }
    if prepared.exists() {
        fs::remove_file(&prepared)
            .map_err(|error| publish_error("removing the stale staging file", &prepared, error))?;
    }
    fs::copy(candidate, &prepared)
        .map_err(|error| publish_error("copying the downloaded binary to", &prepared, error))?;
    fs::File::open(&prepared)
        .and_then(|file| file.sync_all())
        .map_err(|error| publish_error("flushing the staged binary", &prepared, error))?;
    fs::rename(current, &backup)
        .map_err(|error| publish_error("moving the old binary to", &backup, error))?;
    if let Err(publish) = fs::rename(&prepared, current) {
        return match fs::rename(&backup, current) {
            Ok(()) => Err(publish_error("replacing", current, publish)),
            Err(rollback) => Err(UpdateError::caused_by(
                ErrorName::Cli(CliCode::UpdateRollbackFailed),
                format!(
                    "Rift update publish and rollback of `{}` both failed: reinstall Rift from an official release",
                    current.display()
                ),
                RollbackError { publish, rollback },
            )),
        };
    }
    let scheduled = Command::new(current)
        .args([CLEANUP_SUBCOMMAND, &std::process::id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok();
    Ok(if scheduled {
        OldBinaryCleanup::Scheduled
    } else {
        OldBinaryCleanup::Remaining(backup)
    })
}

#[cfg(windows)]
fn sibling(current: &Path, name: &str) -> Result<PathBuf, UpdateError> {
    current
        .parent()
        .map(|parent| parent.join(name))
        .ok_or_else(|| no_parent_error(current))
}

fn no_parent_error(current: &Path) -> UpdateError {
    UpdateError::new(
        ErrorName::Cli(CliCode::UpdatePublishFailed),
        format!(
            "current executable `{}` has no parent directory: install Rift in a regular directory before updating",
            current.display()
        ),
    )
}

fn publish_error(action: &str, path: &Path, error: io::Error) -> UpdateError {
    UpdateError::caused_by(
        ErrorName::Cli(CliCode::UpdatePublishFailed),
        format!(
            "Rift update could not be published: {action} `{}` failed: {error}: ensure the directory is writable and retry `rift update`",
            path.display()
        ),
        error,
    )
}

#[cfg(windows)]
#[derive(Debug)]
struct RollbackError {
    publish: io::Error,
    rollback: io::Error,
}

#[cfg(windows)]
impl fmt::Display for RollbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "publish: {}; rollback: {}",
            self.publish, self.rollback
        )
    }
}

#[cfg(windows)]
impl std::error::Error for RollbackError {}

#[cfg(windows)]
pub(super) fn cleanup_replaced_binary() -> Result<(), UpdateError> {
    let current = std::env::current_exe().map_err(locate_error)?;
    let backup = sibling(&current, WINDOWS_UPDATE_BACKUP_NAME)?;
    for _ in 0..CLEANUP_RETRY_COUNT_MAX {
        match fs::remove_file(&backup) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => std::thread::sleep(CLEANUP_RETRY_DELAY),
        }
    }
    fs::remove_file(&backup).map_err(|error| {
        UpdateError::caused_by(
            ErrorName::Cli(CliCode::UpdateRollbackFailed),
            format!(
                "We were not able to clean up the old binary at `{}`: {error}: delete the file manually",
                backup.display()
            ),
            error,
        )
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::error::Error;
    use std::fs;

    use semver::Version;

    use super::{
        AtomicPublisher, OldBinaryCleanup, Publisher, ReleaseArtifact, UpdateError, UpdateOutcome,
        UpdateSource, parse_release_tag, update_with, validate_candidate, verify_checksum,
    };

    type TestResult = Result<(), Box<dyn Error>>;

    struct FakeSource {
        version: Version,
        stage_calls: Cell<usize>,
    }

    impl UpdateSource for FakeSource {
        async fn latest_version(
            &self,
            _directory: &std::path::Path,
        ) -> Result<Version, UpdateError> {
            Ok(self.version.clone())
        }

        async fn stage(
            &self,
            directory: &std::path::Path,
            _version: Version,
        ) -> Result<std::path::PathBuf, UpdateError> {
            self.stage_calls.set(self.stage_calls.get() + 1);
            let path = directory.join("rift");
            fs::write(&path, b"candidate").map_err(super::archive_error)?;
            Ok(path)
        }
    }

    struct FakePublisher {
        calls: Cell<usize>,
    }

    impl Publisher for FakePublisher {
        async fn publish(
            &self,
            _current: &std::path::Path,
            _candidate: &std::path::Path,
        ) -> Result<OldBinaryCleanup, UpdateError> {
            self.calls.set(self.calls.get() + 1);
            Ok(OldBinaryCleanup::Unnecessary)
        }
    }

    fn version(value: &str) -> Version {
        Version::parse(value).expect("test version must parse")
    }

    #[test]
    fn release_tags_use_standard_stable_semver() {
        assert_eq!(
            parse_release_tag("v0.0.2").expect("tag must parse"),
            version("0.0.2")
        );
        for invalid in [
            "0.0.2",
            "v01.2.3",
            "v1.2.3-beta.1",
            "v1.2.3+build",
            "nightly",
        ] {
            assert!(parse_release_tag(invalid).is_err(), "{invalid} must fail");
        }
    }

    #[test]
    fn artifact_matches_release_contract() {
        super::ReqwestTransport::new().expect("client policy must build");
        let artifact = ReleaseArtifact::new(version("0.0.2"));
        assert!(artifact.archive_name.starts_with("rift-v0.0.2-"));
        assert_eq!(artifact.checksum_name, "rift-v0.0.2-checksums.sha256");
        assert_eq!(artifact.members().len(), 3);
        assert!(
            artifact
                .download_url(&artifact.archive_name)
                .starts_with("https://")
        );
    }

    #[tokio::test]
    async fn current_and_older_releases_never_stage() -> TestResult {
        let publisher = FakePublisher {
            calls: Cell::new(0),
        };
        let current = std::env::current_exe()?;
        for (latest, expected) in [
            ("0.0.2", UpdateOutcome::Current(version("0.0.2"))),
            (
                "0.0.1",
                UpdateOutcome::Unreleased {
                    current: version("0.0.2"),
                    latest: version("0.0.1"),
                },
            ),
        ] {
            let source = FakeSource {
                version: version(latest),
                stage_calls: Cell::new(0),
            };
            let result = update_with(&source, &publisher, &current, version("0.0.2")).await?;
            assert_eq!(result, expected);
            assert_eq!(source.stage_calls.get(), 0);
        }
        assert_eq!(publisher.calls.get(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn newer_release_stages_and_publishes_once() -> TestResult {
        let source = FakeSource {
            version: version("0.0.3"),
            stage_calls: Cell::new(0),
        };
        let publisher = FakePublisher {
            calls: Cell::new(0),
        };
        let outcome = update_with(
            &source,
            &publisher,
            &std::env::current_exe()?,
            version("0.0.2"),
        )
        .await?;
        assert_eq!(
            outcome,
            UpdateOutcome::Updated {
                from: version("0.0.2"),
                to: version("0.0.3"),
                cleanup: OldBinaryCleanup::Unnecessary,
            }
        );
        assert_eq!(source.stage_calls.get(), 1);
        assert_eq!(publisher.calls.get(), 1);
        Ok(())
    }

    #[test]
    fn outcomes_and_errors_expose_stable_user_messages() {
        assert_eq!(
            UpdateOutcome::Current(version("0.0.2")).to_string(),
            "You're already using the latest version of rift 0.0.2 🎉"
        );
        assert_eq!(
            UpdateOutcome::Unreleased {
                current: version("0.0.3"),
                latest: version("0.0.2"),
            }
            .to_string(),
            "You're running a rift version that has never been released. That's a spirit for innovation! (current v0.0.3, latest release v0.0.2)"
        );
        for (cleanup, suffix) in [
            (OldBinaryCleanup::Unnecessary, String::new()),
            (
                OldBinaryCleanup::Scheduled,
                " Rift will automatically clean up the old binary after the update.".to_owned(),
            ),
            (
                OldBinaryCleanup::Remaining(std::path::PathBuf::from("/opt/rift/.rift-update-old.exe")),
                " We were not able to clean up the old binary, please check /opt/rift/.rift-update-old.exe.".to_owned(),
            ),
        ] {
            let outcome = UpdateOutcome::Updated {
                from: version("0.0.1"),
                to: version("0.0.2"),
                cleanup,
            };
            assert_eq!(
                outcome.to_string(),
                format!("Updated Rift from v0.0.1 to v0.0.2.{suffix}")
            );
        }
        assert!(super::error_for_test().source().is_some());
    }

    #[test]
    fn updater_helpers_preserve_error_and_bound_contracts() {
        let cause = || std::io::Error::other("fixture");
        for error in [
            super::checksum_error(cause()),
            super::release_error(cause()),
            super::download_error(cause()),
            super::archive_error(cause()),
            super::publish_error("replacing", std::path::Path::new("/opt/rift/rift"), cause()),
        ] {
            assert!(error.source().is_some());
        }
        for error in [super::download_too_large(1), super::archive_invalid()] {
            assert!(error.source().is_none());
        }
        for mut bytes in [b"".as_slice(), b"ab".as_slice()] {
            assert!(super::copy_bounded(&mut bytes, &mut Vec::new(), 1).is_err());
        }
    }

    #[test]
    fn checksum_requires_one_matching_entry() -> TestResult {
        use sha2::{Digest as _, Sha256};

        let directory = tempfile::tempdir()?;
        let archive = directory.path().join("rift.tar.gz");
        let manifest = directory.path().join("checksums");
        fs::write(&archive, b"archive")?;
        let digest = format!("{:x}", Sha256::digest(b"archive"));
        fs::write(&manifest, format!("{digest}  rift.tar.gz\n"))?;
        verify_checksum(&archive, &manifest, "rift.tar.gz")?;

        for invalid in [
            format!("{digest}  other.tar.gz\n"),
            format!("{}  rift.tar.gz\n", "0".repeat(64)),
        ] {
            fs::write(&manifest, invalid)?;
            assert!(verify_checksum(&archive, &manifest, "rift.tar.gz").is_err());
        }
        Ok(())
    }

    #[test]
    fn candidate_must_be_nonempty_bounded_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let missing = directory.path().join("missing");
        assert!(validate_candidate(&missing).is_err());
        let candidate = directory.path().join("candidate");
        fs::write(&candidate, [])?;
        assert!(validate_candidate(&candidate).is_err());
        fs::File::create(&candidate)?.set_len(super::RELEASE_BINARY_BYTES_MAX + 1)?;
        assert!(validate_candidate(&candidate).is_err());
        Ok(())
    }

    #[cfg(unix)]
    fn release_members(artifact: &ReleaseArtifact) -> Vec<(String, Vec<u8>)> {
        vec![
            (artifact.binary_member(), b"binary".to_vec()),
            (format!("{}/README.md", artifact.root), b"readme".to_vec()),
            (format!("{}/LICENSE.md", artifact.root), b"license".to_vec()),
        ]
    }

    #[cfg(unix)]
    fn write_tar_gz(archive_path: &std::path::Path, entries: &[(String, Vec<u8>)]) -> TestResult {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let raw = fs::File::create(archive_path)?;
        let zipped = GzEncoder::new(raw, Compression::default());
        let mut archive = tar::Builder::new(zipped);
        for (name, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, name, bytes.as_slice())?;
        }
        archive.into_inner()?.finish()?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn tar_archive_extracts_only_exact_release_surface() -> TestResult {
        let directory = tempfile::tempdir()?;
        let artifact = ReleaseArtifact::new(version("0.0.2"));
        let archive_path = directory.path().join(&artifact.archive_name);
        write_tar_gz(&archive_path, &release_members(&artifact))?;
        let candidate = super::extract_candidate(&archive_path, directory.path(), &artifact)?;
        assert_eq!(fs::read(candidate)?, b"binary");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn tar_archives_off_the_release_surface_are_rejected() -> TestResult {
        let artifact = ReleaseArtifact::new(version("0.0.2"));
        let complete = release_members(&artifact);
        let mut extra = complete.clone();
        extra.push((format!("{}/EXTRA.md", artifact.root), b"extra".to_vec()));
        let mut renamed = complete.clone();
        renamed[0].0 = format!("{}/other", artifact.root);
        let mut duplicated = complete.clone();
        duplicated[1] = complete[0].clone();
        let mut empty_binary = complete.clone();
        empty_binary[0].1 = Vec::new();
        for entries in [
            extra,
            renamed,
            duplicated,
            empty_binary,
            complete[..2].to_vec(),
        ] {
            let directory = tempfile::tempdir()?;
            let archive_path = directory.path().join(&artifact.archive_name);
            write_tar_gz(&archive_path, &entries)?;
            let error = super::extract_candidate(&archive_path, directory.path(), &artifact)
                .expect_err("off-surface archive must fail");
            assert!(
                error
                    .to_string()
                    .contains("release archive contents are invalid")
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn tar_directory_entries_are_rejected() -> TestResult {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let directory = tempfile::tempdir()?;
        let artifact = ReleaseArtifact::new(version("0.0.2"));
        let archive_path = directory.path().join(&artifact.archive_name);
        let raw = fs::File::create(&archive_path)?;
        let zipped = GzEncoder::new(raw, Compression::default());
        let mut archive = tar::Builder::new(zipped);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        archive.append_data(&mut header, &artifact.root, b"".as_slice())?;
        archive.into_inner()?.finish()?;
        assert!(super::extract_candidate(&archive_path, directory.path(), &artifact).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_publish_is_atomic_and_preserves_permissions() -> TestResult {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let current = directory.path().join("rift");
        let candidate = directory.path().join("candidate");
        fs::write(&current, b"old")?;
        fs::write(&candidate, b"new")?;
        fs::set_permissions(&current, fs::Permissions::from_mode(0o751))?;
        assert_eq!(
            AtomicPublisher.publish(&current, &candidate).await?,
            OldBinaryCleanup::Unnecessary
        );
        assert_eq!(fs::read(&current)?, b"new");
        assert_eq!(fs::metadata(&current)?.permissions().mode() & 0o777, 0o751);
        Ok(())
    }

    #[test]
    fn error_constructors_name_paths_and_causes() {
        let cause = || std::io::Error::other("fixture cause");

        let locate = super::locate_error(cause());
        assert!(locate.to_string().contains("could not be located"));
        assert!(locate.to_string().contains("fixture cause"));
        assert!(locate.source().is_some());

        let semver_error = Version::parse("bogus").expect_err("bogus version must fail");
        let invalid_version = super::version_error(
            "bogus",
            std::path::Path::new("/opt/rift/rift"),
            semver_error,
        );
        assert!(invalid_version.to_string().contains("`bogus`"));
        assert!(invalid_version.to_string().contains("/opt/rift/rift"));
        assert!(invalid_version.source().is_some());

        let staging = super::staging_error(cause());
        let text = staging.to_string();
        assert!(text.contains(&std::env::temp_dir().display().to_string()));
        assert!(text.contains("bytes free") || text.contains("free space unknown"));
        assert!(text.contains("fixture cause"));

        let publish =
            super::publish_error("replacing", std::path::Path::new("/opt/rift/rift"), cause());
        assert!(
            publish
                .to_string()
                .contains("replacing `/opt/rift/rift` failed")
        );
        assert!(publish.to_string().contains("fixture cause"));

        let no_parent = super::no_parent_error(std::path::Path::new("/"));
        assert!(no_parent.to_string().contains("has no parent directory"));
    }

    #[test]
    fn bounded_file_errors_report_kind_and_size() -> TestResult {
        let directory = tempfile::tempdir()?;

        let missing = directory.path().join("missing");
        let error = super::require_bounded_file(
            super::ErrorName::Cli(rift_core::CliCode::UpdateArchiveInvalid),
            &missing,
            4,
        )
        .expect_err("missing must fail");
        assert!(error.to_string().contains("could not be inspected"));
        assert!(error.to_string().contains("missing"));

        let error = super::require_bounded_file(
            super::ErrorName::Cli(rift_core::CliCode::UpdateArchiveInvalid),
            directory.path(),
            4,
        )
        .expect_err("directory must fail");
        assert!(error.to_string().contains("is not a regular file"));
        assert!(error.to_string().contains(super::ISSUE_TRACKER_URL));

        let oversized = directory.path().join("oversized");
        fs::write(&oversized, b"12345")?;
        let error = super::require_bounded_file(
            super::ErrorName::Cli(rift_core::CliCode::UpdateArchiveInvalid),
            &oversized,
            4,
        )
        .expect_err("oversize must fail");
        assert!(error.to_string().contains("incorrect size of 5 bytes"));
        assert!(error.to_string().contains("between 1 and 4 bytes"));
        Ok(())
    }

    #[test]
    fn update_errors_carry_registry_codes() -> TestResult {
        let directory = tempfile::tempdir()?;
        let missing = directory.path().join("missing");

        let release_tag = parse_release_tag("vinvalid").expect_err("tag must be invalid");
        assert_eq!(release_tag.descriptor().code(), "update_release_invalid");

        let archive = super::require_bounded_file(
            super::ErrorName::Cli(rift_core::CliCode::UpdateArchiveInvalid),
            &missing,
            4,
        )
        .expect_err("missing must fail");
        assert_eq!(archive.descriptor().code(), "update_archive_invalid");

        let staging = super::staging_error(std::io::Error::other("fixture"));
        assert_eq!(staging.descriptor().code(), "update_staging_failed");

        let download = super::download_error(std::io::Error::other("fixture"));
        assert_eq!(download.descriptor().code(), "update_download_failed");

        let checksum = super::checksum_error(std::io::Error::other("fixture"));
        assert_eq!(checksum.descriptor().code(), "update_checksum_mismatch");

        let publish = super::publish_error(
            "replacing",
            std::path::Path::new("/opt/rift/rift"),
            std::io::Error::other("fixture"),
        );
        assert_eq!(publish.descriptor().code(), "update_publish_failed");

        let locate = super::locate_error(std::io::Error::other("fixture"));
        assert_eq!(locate.descriptor().code(), "update_binary_invalid");
        Ok(())
    }

    #[test]
    fn checksum_mismatch_reports_both_digests() -> TestResult {
        use sha2::{Digest as _, Sha256};

        let directory = tempfile::tempdir()?;
        let archive = directory.path().join("rift.tar.gz");
        let manifest = directory.path().join("checksums");
        fs::write(&archive, b"archive")?;
        let expected = "0".repeat(64);
        fs::write(&manifest, format!("{expected}  rift.tar.gz\n"))?;
        let error =
            verify_checksum(&archive, &manifest, "rift.tar.gz").expect_err("mismatch must fail");
        let text = error.to_string();
        let actual = format!("{:x}", Sha256::digest(b"archive"));
        assert!(text.starts_with("the downloaded release does not match its published checksum"));
        assert!(text.contains(&expected));
        assert!(text.contains(&actual));
        assert!(text.contains(super::ISSUE_TRACKER_URL));
        Ok(())
    }

    #[test]
    fn checksum_manifests_reject_malformed_entries() -> TestResult {
        use sha2::{Digest as _, Sha256};
        use std::fmt::Write as _;

        let directory = tempfile::tempdir()?;
        let archive = directory.path().join("rift.tar.gz");
        let manifest = directory.path().join("checksums");
        fs::write(&archive, b"archive")?;
        let digest = format!("{:x}", Sha256::digest(b"archive"));
        let mut overflowing = String::new();
        for index in 0..=super::CHECKSUM_ENTRY_COUNT_MAX {
            writeln!(overflowing, "{digest}  other-{index}.tar.gz")?;
        }
        for invalid in [
            "no separator\n".to_owned(),
            format!("{}  rift.tar.gz\n", "0".repeat(8)),
            format!("{digest}  rift.tar.gz\n{digest}  rift.tar.gz\n"),
            overflowing,
        ] {
            fs::write(&manifest, invalid)?;
            let error = verify_checksum(&archive, &manifest, "rift.tar.gz")
                .expect_err("malformed manifest must fail");
            assert!(
                error
                    .to_string()
                    .contains("release checksum manifest is invalid")
            );
        }
        Ok(())
    }

    struct FixtureTransport {
        metadata: Vec<u8>,
        manifest: Vec<u8>,
        archive: Vec<u8>,
    }

    impl super::DownloadTransport for FixtureTransport {
        async fn download(
            &self,
            url: &str,
            destination: &std::path::Path,
            _bytes_max: u64,
        ) -> Result<(), UpdateError> {
            let bytes = if url == rift_core::constants::RELEASE_API_URL {
                &self.metadata
            } else if url.ends_with(".sha256") {
                &self.manifest
            } else {
                &self.archive
            };
            if bytes.is_empty() {
                return Err(super::download_error(std::io::Error::other(
                    "fixture download unavailable",
                )));
            }
            fs::write(destination, bytes).map_err(super::download_error)
        }
    }

    #[tokio::test]
    async fn github_source_propagates_download_failures() -> TestResult {
        for (manifest, archive) in [(Vec::new(), b"x".to_vec()), (b"x".to_vec(), Vec::new())] {
            let staging = tempfile::tempdir()?;
            let source = super::GitHubReleaseSource {
                transport: FixtureTransport {
                    metadata: Vec::new(),
                    manifest,
                    archive,
                },
            };
            let error = source
                .stage(staging.path(), version("0.0.3"))
                .await
                .expect_err("unavailable release asset must fail");
            assert!(error.to_string().contains("release download failed"));
        }
        Ok(())
    }

    #[tokio::test]
    async fn github_source_parses_latest_release_metadata() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source = super::GitHubReleaseSource {
            transport: FixtureTransport {
                metadata: br#"{"tag_name":"v0.0.3"}"#.to_vec(),
                manifest: Vec::new(),
                archive: Vec::new(),
            },
        };
        assert_eq!(
            source.latest_version(directory.path()).await?,
            version("0.0.3")
        );

        let source = super::GitHubReleaseSource {
            transport: FixtureTransport {
                metadata: b"not json".to_vec(),
                manifest: Vec::new(),
                archive: Vec::new(),
            },
        };
        let error = source
            .latest_version(directory.path())
            .await
            .expect_err("malformed metadata must fail");
        assert!(
            error
                .to_string()
                .contains("latest release metadata is invalid")
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn github_source_stages_checksummed_candidate() -> TestResult {
        use sha2::{Digest as _, Sha256};

        let build = tempfile::tempdir()?;
        let artifact = ReleaseArtifact::new(version("0.0.3"));
        let archive_path = build.path().join(&artifact.archive_name);
        write_tar_gz(&archive_path, &release_members(&artifact))?;
        let archive = fs::read(&archive_path)?;
        let digest = format!("{:x}", Sha256::digest(&archive));
        let manifest = format!("{digest}  {}\n", artifact.archive_name).into_bytes();

        let staging = tempfile::tempdir()?;
        let source = super::GitHubReleaseSource {
            transport: FixtureTransport {
                metadata: Vec::new(),
                manifest,
                archive: archive.clone(),
            },
        };
        let candidate = source.stage(staging.path(), version("0.0.3")).await?;
        assert_eq!(fs::read(candidate)?, b"binary");

        let staging = tempfile::tempdir()?;
        let source = super::GitHubReleaseSource {
            transport: FixtureTransport {
                metadata: Vec::new(),
                manifest: format!("{}  {}\n", "0".repeat(64), artifact.archive_name).into_bytes(),
                archive,
            },
        };
        let error = source
            .stage(staging.path(), version("0.0.3"))
            .await
            .expect_err("corrupted archive must fail");
        assert!(
            error
                .to_string()
                .starts_with("the downloaded release does not match its published checksum")
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_publish_reports_failing_operation_and_path() -> TestResult {
        let directory = tempfile::tempdir()?;
        let current = directory.path().join("rift");
        fs::write(&current, b"old")?;

        let missing = directory.path().join("missing");
        let error = AtomicPublisher
            .publish(&current, &missing)
            .await
            .expect_err("missing candidate must fail");
        assert!(error.to_string().contains("opening the downloaded binary"));
        assert!(error.to_string().contains("missing"));

        let empty = directory.path().join("empty");
        fs::write(&empty, [])?;
        let error = AtomicPublisher
            .publish(&current, &empty)
            .await
            .expect_err("empty candidate must fail");
        assert!(
            error
                .to_string()
                .contains("copying the downloaded binary into")
        );

        let ghost = directory.path().join("ghost");
        let candidate = directory.path().join("candidate");
        fs::write(&candidate, b"new")?;
        let error = AtomicPublisher
            .publish(&ghost, &candidate)
            .await
            .expect_err("missing current binary must fail");
        assert!(error.to_string().contains("reading permissions of"));

        let error = AtomicPublisher
            .publish(std::path::Path::new("/"), &candidate)
            .await
            .expect_err("rootless current path must fail");
        assert!(error.to_string().contains("has no parent directory"));
        Ok(())
    }

    #[tokio::test]
    async fn transport_download_enforces_status_and_bounds() -> TestResult {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let responses: Vec<Vec<u8>> = vec![
            b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\nconnection: close\r\n\r\npayload".to_vec(),
            b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\nconnection: close\r\n\r\npayload".to_vec(),
            b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\npayload".to_vec(),
            b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: close\r\n\r\nshort".to_vec(),
            b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec(),
            b"HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:9/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                .to_vec(),
        ];
        let server = std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept must succeed");
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request);
                stream.write_all(&response).expect("response must send");
            }
        });

        let transport = super::ReqwestTransport::new()?;
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("payload");
        super::DownloadTransport::download(
            &transport,
            &format!("http://{address}/ok"),
            &destination,
            64,
        )
        .await?;
        assert_eq!(fs::read(&destination)?, b"payload");

        let error = super::DownloadTransport::download(
            &transport,
            &format!("http://{address}/large"),
            &destination,
            3,
        )
        .await
        .expect_err("oversized download must fail");
        assert!(error.to_string().contains("exceeded 3 bytes"));

        let error = super::DownloadTransport::download(
            &transport,
            &format!("http://{address}/unsized"),
            &destination,
            3,
        )
        .await
        .expect_err("unsized oversized body must stop at the counting ceiling");
        assert!(error.to_string().contains("exceeded 3 bytes"));

        let error = super::DownloadTransport::download(
            &transport,
            &format!("http://{address}/truncated"),
            &destination,
            256,
        )
        .await
        .expect_err("body shorter than its declared length must fail");
        assert!(error.to_string().contains("release download failed"));

        let error = super::DownloadTransport::download(
            &transport,
            &format!("http://{address}/absent"),
            &destination,
            64,
        )
        .await
        .expect_err("missing asset must fail");
        assert!(error.to_string().contains("release download failed"));

        let error = super::DownloadTransport::download(
            &transport,
            &format!("http://{address}/redirect"),
            &destination,
            64,
        )
        .await
        .expect_err("insecure redirect must stop with an empty body");
        assert!(error.to_string().contains("was empty or exceeded"));

        server.join().expect("server thread must finish");
        Ok(())
    }

    #[tokio::test]
    async fn transport_follows_secure_redirects_within_hop_budget() -> TestResult {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept must succeed");
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nlocation: https://127.0.0.1:9/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .expect("response must send");
        });

        let transport = super::ReqwestTransport::new()?;
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("payload");
        let error = super::DownloadTransport::download(
            &transport,
            &format!("http://{address}/redirect"),
            &destination,
            64,
        )
        .await
        .expect_err("followed secure redirect must fail to connect");
        assert!(error.to_string().contains("release download failed"));
        server.join().expect("server thread must finish");
        Ok(())
    }

    #[test]
    fn body_chunks_are_counted_within_budget() {
        assert_eq!(super::bytes_within_budget(5, 3), 3);
        assert_eq!(super::bytes_within_budget(5, 5), 5);
        assert_eq!(super::bytes_within_budget(5, 9), 5);
        assert_eq!(super::bytes_within_budget(0, 9), 0);
    }

    #[tokio::test]
    #[should_panic(expected = "fixture blocking panic")]
    async fn blocking_stage_panics_resume_on_the_caller() {
        let _ =
            super::run_blocking(|| -> Result<(), UpdateError> { panic!("fixture blocking panic") })
                .await;
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "probe run by staging_error_reports_unknown_free_space in a child process"]
    fn staging_error_free_space_probe() {
        let error = super::staging_error(std::io::Error::other("fixture"));
        assert!(error.to_string().contains("free space unknown"));
    }

    #[cfg(unix)]
    #[test]
    fn staging_error_reports_unknown_free_space() -> TestResult {
        let output = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "update::tests::staging_error_free_space_probe",
                "--ignored",
            ])
            .env("TMPDIR", "/rift-nonexistent-staging-root")
            .output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "probe must pass: {stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
}
