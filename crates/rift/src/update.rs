//! Bounded, checksummed self-update for official Rift releases.

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::{Command, Stdio};
use std::time::Duration;

const RELEASE_API_URL: &str = "https://api.github.com/repos/volarized/rift/releases/latest";
const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/volarized/rift/releases/download";
const RELEASE_METADATA_BYTES_MAX: u64 = 1024 * 1024;
const CHECKSUM_MANIFEST_BYTES_MAX: u64 = 16 * 1024;
const RELEASE_ARCHIVE_BYTES_MAX: u64 = 128 * 1024 * 1024;
const RELEASE_BINARY_BYTES_MAX: u64 = 128 * 1024 * 1024;
const RELEASE_DOCUMENT_BYTES_MAX: u64 = 4 * 1024 * 1024;
const RELEASE_MEMBER_COUNT: usize = 3;
const CHECKSUM_ENTRY_COUNT_MAX: usize = 16;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(5);

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
    Updated {
        from: Version,
        to: Version,
        cleanup_pending: bool,
    },
}

impl fmt::Display for UpdateOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Current(version) => write!(formatter, "Rift v{version} is already current."),
            Self::Updated {
                from,
                to,
                cleanup_pending,
            } => write!(
                formatter,
                "Updated Rift from v{from} to v{to}{}",
                if *cleanup_pending {
                    "; old binary cleanup remains pending."
                } else {
                    "."
                }
            ),
        }
    }
}

/// Opaque updater failure.
#[derive(Debug)]
pub(super) struct UpdateError {
    message: &'static str,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl UpdateError {
    fn new(message: &'static str) -> Self {
        Self {
            message,
            source: None,
        }
    }

    fn caused_by(
        message: &'static str,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message,
            source: Some(Box::new(source)),
        }
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
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

    fn members(&self) -> [String; RELEASE_MEMBER_COUNT] {
        [
            self.binary_member(),
            format!("{}/README.md", self.root),
            format!("{}/LICENSE.md", self.root),
        ]
    }

    fn download_url(&self, name: &str) -> String {
        format!("{RELEASE_DOWNLOAD_BASE}/v{}/{name}", self.version)
    }
}

trait DownloadTransport {
    fn download(&self, url: &str, destination: &Path, bytes_max: u64) -> Result<(), UpdateError>;
}

trait UpdateSource {
    fn latest_version(&self, directory: &Path) -> Result<Version, UpdateError>;
    fn stage(&self, directory: &Path, version: Version) -> Result<PathBuf, UpdateError>;
}

trait Publisher {
    fn publish(&self, current: &Path, candidate: &Path) -> Result<bool, UpdateError>;
}

struct ReqwestTransport {
    client: reqwest::blocking::Client,
}
struct GitHubReleaseSource<T> {
    transport: T,
}
struct AtomicPublisher;

pub(super) fn update() -> Result<UpdateOutcome, UpdateError> {
    let current = std::env::current_exe()
        .map_err(|error| UpdateError::caused_by("current Rift executable is unavailable", error))?;
    let current_version = Version::parse(env!("CARGO_PKG_VERSION")).map_err(version_error)?;
    update_with(
        &GitHubReleaseSource {
            transport: ReqwestTransport::new()?,
        },
        &AtomicPublisher,
        &current,
        current_version,
    )
}

fn update_with(
    source: &impl UpdateSource,
    publisher: &impl Publisher,
    current_path: &Path,
    current_version: Version,
) -> Result<UpdateOutcome, UpdateError> {
    let staging = tempfile::tempdir().map_err(|error| {
        UpdateError::caused_by("update staging directory could not be created", error)
    })?;
    let latest_version = source.latest_version(staging.path())?;
    match latest_version.cmp(&current_version) {
        Ordering::Equal => Ok(UpdateOutcome::Current(current_version)),
        Ordering::Less => Err(UpdateError::new(
            "latest release is older than current Rift",
        )),
        Ordering::Greater => {
            let candidate = source.stage(staging.path(), latest_version.clone())?;
            let cleanup_pending = publisher.publish(current_path, &candidate)?;
            Ok(UpdateOutcome::Updated {
                from: current_version,
                to: latest_version,
                cleanup_pending,
            })
        }
    }
}

impl ReqwestTransport {
    fn new() -> Result<Self, UpdateError> {
        let redirects = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 || attempt.url().scheme() != "https" {
                attempt.stop()
            } else {
                attempt.follow()
            }
        });
        let client = reqwest::blocking::Client::builder()
            .redirect(redirects)
            .timeout(DOWNLOAD_TIMEOUT)
            .min_tls_version(reqwest::tls::Version::TLS_1_2)
            .user_agent("rift-updater")
            .build()
            .map_err(download_error)?;
        Ok(Self { client })
    }
}

impl DownloadTransport for ReqwestTransport {
    fn download(&self, url: &str, destination: &Path, bytes_max: u64) -> Result<(), UpdateError> {
        let mut response = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(download_error)?;
        if response
            .content_length()
            .is_some_and(|length| length > bytes_max)
        {
            return Err(download_too_large());
        }
        let mut output = fs::File::create(destination).map_err(download_error)?;
        let copied = io::copy(&mut response.by_ref().take(bytes_max + 1), &mut output)
            .map_err(download_error)?;
        if copied == 0 || copied > bytes_max {
            return Err(download_too_large());
        }
        output.sync_all().map_err(download_error)?;
        Ok(())
    }
}

impl<T: DownloadTransport> UpdateSource for GitHubReleaseSource<T> {
    fn latest_version(&self, directory: &Path) -> Result<Version, UpdateError> {
        let metadata_path = directory.join("latest.json");
        self.transport
            .download(RELEASE_API_URL, &metadata_path, RELEASE_METADATA_BYTES_MAX)?;
        let bytes = fs::read(metadata_path).map_err(release_error)?;
        let release: LatestRelease = serde_json::from_slice(&bytes).map_err(release_error)?;
        parse_release_tag(&release.tag_name)
    }

    fn stage(&self, directory: &Path, version: Version) -> Result<PathBuf, UpdateError> {
        let artifact = ReleaseArtifact::new(version);
        let manifest_path = directory.join(&artifact.checksum_name);
        let archive_path = directory.join(&artifact.archive_name);
        self.transport.download(
            &artifact.download_url(&artifact.checksum_name),
            &manifest_path,
            CHECKSUM_MANIFEST_BYTES_MAX,
        )?;
        self.transport.download(
            &artifact.download_url(&artifact.archive_name),
            &archive_path,
            RELEASE_ARCHIVE_BYTES_MAX,
        )?;
        verify_checksum(&archive_path, &manifest_path, &artifact.archive_name)?;
        extract_candidate(&archive_path, directory, &artifact)
    }
}

fn parse_release_tag(tag: &str) -> Result<Version, UpdateError> {
    let value = tag
        .strip_prefix('v')
        .ok_or_else(|| UpdateError::new("release version is invalid"))?;
    let version = Version::parse(value).map_err(version_error)?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err(UpdateError::new("release version is invalid"));
    }
    Ok(version)
}

fn version_error(error: semver::Error) -> UpdateError {
    UpdateError::caused_by("release version is invalid", error)
}

#[cfg(test)]
pub(super) fn error_for_test() -> UpdateError {
    version_error(Version::parse("invalid").expect_err("fixture must be invalid"))
}

fn require_bounded_file(path: &Path, bytes_max: u64) -> Result<u64, UpdateError> {
    let metadata = fs::metadata(path)
        .map_err(|error| UpdateError::caused_by("release file is unavailable", error))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > bytes_max {
        return Err(UpdateError::new("release file has invalid type or size"));
    }
    Ok(metadata.len())
}

fn verify_checksum(archive: &Path, manifest: &Path, archive_name: &str) -> Result<(), UpdateError> {
    require_bounded_file(manifest, CHECKSUM_MANIFEST_BYTES_MAX)?;
    let content = fs::read_to_string(manifest).map_err(checksum_error)?;
    let mut expected = None;
    for (index, line) in content.lines().enumerate() {
        if index >= CHECKSUM_ENTRY_COUNT_MAX {
            return Err(checksum_invalid());
        }
        let (digest, name) = line.split_once("  ").ok_or_else(checksum_invalid)?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(checksum_invalid());
        }
        if name == archive_name && expected.replace(digest).is_some() {
            return Err(checksum_invalid());
        }
    }
    let expected = expected.ok_or_else(checksum_invalid)?;
    let actual = sha256(archive)?;
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(UpdateError::new("release archive checksum mismatch"));
    }
    Ok(())
}

fn sha256(path: &Path) -> Result<String, UpdateError> {
    require_bounded_file(path, RELEASE_ARCHIVE_BYTES_MAX)?;
    let mut file = fs::File::open(path).map_err(checksum_error)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(checksum_error)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn checksum_invalid() -> UpdateError {
    UpdateError::new("release checksum manifest is invalid")
}

fn checksum_error(error: io::Error) -> UpdateError {
    UpdateError::caused_by("release checksum could not be verified", error)
}

fn release_error(error: impl std::error::Error + Send + Sync + 'static) -> UpdateError {
    UpdateError::caused_by("latest Rift release is invalid", error)
}

fn download_error(error: impl std::error::Error + Send + Sync + 'static) -> UpdateError {
    UpdateError::caused_by("release download failed", error)
}

fn download_too_large() -> UpdateError {
    UpdateError::new("release download has invalid size")
}

fn copy_bounded(
    source: &mut impl Read,
    destination: &mut impl Write,
    bytes_max: u64,
) -> Result<(), UpdateError> {
    let copied = io::copy(&mut source.take(bytes_max + 1), destination).map_err(archive_error)?;
    if copied == 0 || copied > bytes_max {
        return Err(UpdateError::new("release archive member has invalid size"));
    }
    Ok(())
}

fn extract_member(
    entry: &mut impl Read,
    path: &Path,
    size: u64,
    expected: &[String; RELEASE_MEMBER_COUNT],
    seen: &mut [bool; RELEASE_MEMBER_COUNT],
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
    seen: [bool; RELEASE_MEMBER_COUNT],
    candidate: PathBuf,
) -> Result<PathBuf, UpdateError> {
    if seen != [true; RELEASE_MEMBER_COUNT] {
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
    let mut seen = [false; RELEASE_MEMBER_COUNT];
    let candidate = directory.join(ReleaseArtifact::binary_name());
    let entries = archive.entries().map_err(archive_error)?;
    for (index, entry) in entries.enumerate() {
        if index >= RELEASE_MEMBER_COUNT {
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
    if archive.len() != RELEASE_MEMBER_COUNT {
        return Err(archive_invalid());
    }
    let expected = artifact.members();
    let mut seen = [false; RELEASE_MEMBER_COUNT];
    let candidate = directory.join(ReleaseArtifact::binary_name());
    for index in 0..RELEASE_MEMBER_COUNT {
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
    require_bounded_file(path, RELEASE_BINARY_BYTES_MAX)?;
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
    UpdateError::new("release archive contents are invalid")
}

fn archive_error(error: impl std::error::Error + Send + Sync + 'static) -> UpdateError {
    UpdateError::caused_by("release archive could not be extracted", error)
}

impl Publisher for AtomicPublisher {
    fn publish(&self, current: &Path, candidate: &Path) -> Result<bool, UpdateError> {
        publish_candidate(current, candidate)
    }
}

#[cfg(unix)]
fn publish_candidate(current: &Path, candidate: &Path) -> Result<bool, UpdateError> {
    let parent = current
        .parent()
        .ok_or_else(|| UpdateError::new("current executable has no parent"))?;
    let mut prepared = tempfile::NamedTempFile::new_in(parent).map_err(publish_error)?;
    let mut source = fs::File::open(candidate).map_err(publish_error)?;
    copy_bounded(
        &mut source,
        prepared.as_file_mut(),
        RELEASE_BINARY_BYTES_MAX,
    )
    .map_err(|error| UpdateError::caused_by("Rift update could not be published", error))?;
    let permissions = fs::metadata(current).map_err(publish_error)?.permissions();
    prepared
        .as_file()
        .set_permissions(permissions)
        .map_err(publish_error)?;
    prepared.as_file().sync_all().map_err(publish_error)?;
    prepared
        .persist(current)
        .map_err(|error| publish_error(error.error))?;
    Ok(false)
}

#[cfg(windows)]
fn publish_candidate(current: &Path, candidate: &Path) -> Result<bool, UpdateError> {
    let prepared = sibling(current, ".rift-update-new.exe")?;
    let backup = sibling(current, ".rift-update-old.exe")?;
    if backup.exists() {
        return Err(UpdateError::new("another Rift update is pending cleanup"));
    }
    if prepared.exists() {
        fs::remove_file(&prepared).map_err(publish_error)?;
    }
    fs::copy(candidate, &prepared).map_err(publish_error)?;
    fs::File::open(&prepared)
        .and_then(|file| file.sync_all())
        .map_err(publish_error)?;
    fs::rename(current, &backup).map_err(publish_error)?;
    if let Err(publish) = fs::rename(&prepared, current) {
        return match fs::rename(&backup, current) {
            Ok(()) => Err(publish_error(publish)),
            Err(rollback) => Err(UpdateError::caused_by(
                "Rift update publish and rollback failed",
                RollbackError { publish, rollback },
            )),
        };
    }
    let scheduled = Command::new(current)
        .args(["__cleanup-update", &std::process::id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok();
    Ok(!scheduled)
}

#[cfg(windows)]
fn sibling(current: &Path, name: &str) -> Result<PathBuf, UpdateError> {
    current
        .parent()
        .map(|parent| parent.join(name))
        .ok_or_else(|| UpdateError::new("current executable has no parent"))
}

fn publish_error(error: io::Error) -> UpdateError {
    UpdateError::caused_by("Rift update could not be published", error)
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
    let current = std::env::current_exe().map_err(publish_error)?;
    let backup = sibling(&current, ".rift-update-old.exe")?;
    for _ in 0..40 {
        match fs::remove_file(&backup) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    fs::remove_file(backup).map_err(publish_error)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::error::Error;
    use std::fs;

    use semver::Version;

    use super::{
        AtomicPublisher, Publisher, ReleaseArtifact, UpdateError, UpdateOutcome, UpdateSource,
        parse_release_tag, update_with, validate_candidate, verify_checksum,
    };

    type TestResult = Result<(), Box<dyn Error>>;

    struct FakeSource {
        version: Version,
        stage_calls: Cell<usize>,
    }

    impl UpdateSource for FakeSource {
        fn latest_version(&self, _directory: &std::path::Path) -> Result<Version, UpdateError> {
            Ok(self.version.clone())
        }

        fn stage(
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
        fn publish(
            &self,
            _current: &std::path::Path,
            _candidate: &std::path::Path,
        ) -> Result<bool, UpdateError> {
            self.calls.set(self.calls.get() + 1);
            Ok(false)
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

    #[test]
    fn current_and_older_releases_never_stage() -> TestResult {
        let publisher = FakePublisher {
            calls: Cell::new(0),
        };
        let current = std::env::current_exe()?;
        for (latest, is_current) in [("0.0.2", true), ("0.0.1", false)] {
            let source = FakeSource {
                version: version(latest),
                stage_calls: Cell::new(0),
            };
            let result = update_with(&source, &publisher, &current, version("0.0.2"));
            if is_current {
                assert_eq!(result?, UpdateOutcome::Current(version("0.0.2")));
            } else {
                assert_eq!(
                    result.expect_err("downgrade must fail").to_string(),
                    "latest release is older than current Rift"
                );
            }
            assert_eq!(source.stage_calls.get(), 0);
        }
        assert_eq!(publisher.calls.get(), 0);
        Ok(())
    }

    #[test]
    fn newer_release_stages_and_publishes_once() -> TestResult {
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
        )?;
        assert_eq!(
            outcome,
            UpdateOutcome::Updated {
                from: version("0.0.2"),
                to: version("0.0.3"),
                cleanup_pending: false,
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
            "Rift v0.0.2 is already current."
        );
        for (cleanup_pending, suffix) in [
            (false, "."),
            (true, "; old binary cleanup remains pending."),
        ] {
            let outcome = UpdateOutcome::Updated {
                from: version("0.0.1"),
                to: version("0.0.2"),
                cleanup_pending,
            };
            assert_eq!(
                outcome.to_string(),
                format!("Updated Rift from v0.0.1 to v0.0.2{suffix}")
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
            super::publish_error(cause()),
        ] {
            assert!(error.source().is_some());
        }
        for error in [super::download_too_large(), super::archive_invalid()] {
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
    #[test]
    fn tar_archive_extracts_only_exact_release_surface() -> TestResult {
        use flate2::Compression;
        use flate2::write::GzEncoder;

        let directory = tempfile::tempdir()?;
        let artifact = ReleaseArtifact::new(version("0.0.2"));
        let archive_path = directory.path().join(&artifact.archive_name);
        let raw = fs::File::create(&archive_path)?;
        let zipped = GzEncoder::new(raw, Compression::default());
        let mut archive = tar::Builder::new(zipped);
        for (name, bytes) in [
            (artifact.binary_member(), b"binary".as_slice()),
            (format!("{}/README.md", artifact.root), b"readme".as_slice()),
            (
                format!("{}/LICENSE.md", artifact.root),
                b"license".as_slice(),
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, name, bytes)?;
        }
        let zipped = archive.into_inner()?;
        zipped.finish()?;
        let candidate = super::extract_candidate(&archive_path, directory.path(), &artifact)?;
        assert_eq!(fs::read(candidate)?, b"binary");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unix_publish_is_atomic_and_preserves_permissions() -> TestResult {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let current = directory.path().join("rift");
        let candidate = directory.path().join("candidate");
        fs::write(&current, b"old")?;
        fs::write(&candidate, b"new")?;
        fs::set_permissions(&current, fs::Permissions::from_mode(0o751))?;
        assert!(!AtomicPublisher.publish(&current, &candidate)?);
        assert_eq!(fs::read(&current)?, b"new");
        assert_eq!(fs::metadata(&current)?.permissions().mode() & 0o777, 0o751);
        Ok(())
    }
}
