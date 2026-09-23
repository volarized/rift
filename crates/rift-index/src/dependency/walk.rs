//! The walk that selects the files spelling one cataloged package's API.
//!
//! `PackageLanguage` holds each language's file selection and its
//! public-declaration rule; the package index consults the rule through
//! `public_qualified_names`.

use std::ffi::OsStr;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use ignore::{DirEntry, Walk, WalkBuilder};
use rift_analysis::ExactPackageLimits;
use rift_analysis::archive::{
    ArchiveDigest, ArchiveFiles, ArchiveFormat, ArchiveLimits, read_archive,
};
use rift_analysis::{PackageLanguage, documentation_format};
use rift_core::{LoopBudget, ProjectPath, bounded_for};
use rift_dependency::{CatalogEntry, CatalogSource};
use rift_protocol::documentation::DocumentationSourceFormat;
use rift_protocol::read::PackageIdentity;

use super::failure::{PackageIndexError, PackageIndexFault, PackageIndexViolation};
use super::{
    DIRECTORY_DEPTH_MAX_FIELD, DependencyIndexLimits, PACKAGE_BYTES_MAX_FIELD,
    PACKAGE_FILES_MAX_FIELD, WALK_ENTRIES_MAX_FIELD,
};
use crate::workspace::{TextSourceFile, relative_path};

/// Directory names a package walk never descends into.
const SKIPPED_DIRECTORY_NAMES: &[&str] = &[
    "tests",
    "benches",
    "examples",
    "target",
    "node_modules",
    "__pycache__",
    ".git",
];
/// Suffix a skipped directory name ends in once its surrounding underscores are trimmed.
/// `integration_tests` and `__tests__` both match.
const TESTS_DIRECTORY_SUFFIX: &str = "tests";
/// Python stub suffix, preferred over modules when the package ships any.
const PYTHON_STUB_SUFFIX: &str = ".pyi";

/// A `usize` count as the `u64` a limit refusal reports.
fn count_u64(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// The files one cataloged package's API is read from: package-relative, UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageFiles {
    files: Vec<TextSourceFile>,
    skipped_binary: usize,
    limits: ExactPackageLimits,
}

impl PackageFiles {
    /// The selected files, with the count of files skipped as binary.
    #[cfg(test)]
    pub(crate) fn new(files: Vec<TextSourceFile>, skipped_binary: usize) -> Self {
        let files_max = u32::try_from(files.len()).unwrap_or(u32::MAX);
        let bytes_max = files
            .iter()
            .map(|file| count_u64(file.content().len()))
            .fold(0, u64::saturating_add);
        Self::with_limits(
            files,
            skipped_binary,
            ExactPackageLimits::new(files_max, bytes_max),
        )
    }

    fn with_limits(
        files: Vec<TextSourceFile>,
        skipped_binary: usize,
        limits: ExactPackageLimits,
    ) -> Self {
        Self {
            files,
            skipped_binary,
            limits,
        }
    }

    pub(crate) const fn analysis_limits(&self) -> ExactPackageLimits {
        self.limits
    }

    /// Every selected file, in walk order.
    #[must_use]
    pub fn files(&self) -> &[TextSourceFile] {
        &self.files
    }

    /// How many files were selected.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// The bytes every selected file holds together.
    #[must_use]
    pub fn byte_count(&self) -> u64 {
        self.files
            .iter()
            .map(|file| count_u64(file.content().len()))
            .fold(0, u64::saturating_add)
    }

    /// How many selected files held NUL bytes or invalid UTF-8 and were skipped.
    #[must_use]
    pub const fn skipped_binary(&self) -> usize {
        self.skipped_binary
    }
}

/// One selected file before it is read: where it sits and its package-relative path.
#[derive(Debug)]
struct Candidate {
    absolute: PathBuf,
    relative: ProjectPath,
    /// The file's size when the walk saw it, summed before any file is read.
    bytes: u64,
}

impl Candidate {
    fn is_python_stub(&self) -> bool {
        self.relative.as_str().ends_with(PYTHON_STUB_SUFFIX)
    }

    fn is_documentation(&self) -> bool {
        is_selected_documentation(self.relative.as_str())
    }
}

fn is_selected_documentation(path: &str) -> bool {
    documentation_format(path).is_some_and(|format| format != DocumentationSourceFormat::Text)
}

fn selected_files(language: PackageLanguage, candidates: Vec<Candidate>) -> Vec<Candidate> {
    let has_stub = candidates
        .iter()
        .any(|candidate| !candidate.is_documentation() && candidate.is_python_stub());
    let mut selected: Vec<Candidate> = candidates
        .into_iter()
        .filter(|candidate| {
            candidate.is_documentation()
                || language != PackageLanguage::Python
                || candidate.is_python_stub() == has_stub
        })
        .collect();
    selected.sort_by(|left, right| left.relative.cmp(&right.relative));
    selected
}

/// Reads the files that spell one cataloged package's API.
///
/// The source root is a directory walked in file-name order, or one file that
/// is the whole package. Directories named in `SKIPPED_DIRECTORY_NAMES`, or
/// ending in `tests`, and every symlink are left out. Files are selected by the
/// entry's language (`PackageLanguage`); a selected file holding NUL bytes or
/// invalid UTF-8 is skipped and counted. The walk stops at `directory_depth_max`
/// and `walk_entries_max`, and the package refuses past `package_files_max` and
/// `package_bytes_max`; every refusal names the package.
///
/// # Errors
///
/// Returns [`PackageIndexError`] when the entry has no source root or no
/// supported language, when the root or a file cannot be read, when a path
/// is not a valid project path, or when a bound is crossed.
pub fn package_files(
    entry: &CatalogEntry,
    limits: &DependencyIndexLimits,
) -> Result<PackageFiles, PackageIndexError> {
    let package = entry.identity();
    let language = PackageLanguage::for_language(entry.language()).ok_or_else(|| {
        PackageIndexFault::new(PackageIndexViolation::LanguageUnsupported, package)
    })?;
    match entry.source() {
        Some(CatalogSource::Directory(root)) => {
            package_directory_files(root, language, package, limits)
        }
        Some(CatalogSource::Archive { path, sha256 }) => {
            package_archive_files(path, sha256, language, package, limits)
        }
        None => {
            Err(PackageIndexFault::new(PackageIndexViolation::SourceRootMissing, package).into())
        }
    }
}

fn package_directory_files(
    root: &Path,
    language: PackageLanguage,
    package: &PackageIdentity,
    limits: &DependencyIndexLimits,
) -> Result<PackageFiles, PackageIndexError> {
    let candidates = selected_files(language, candidates_below(root, language, package, limits)?);
    if candidates.len() > limits.package_files_max {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::PackageFilesExceeded, package)
                .at(root)
                .breached(
                    PACKAGE_FILES_MAX_FIELD,
                    count_u64(limits.package_files_max),
                    count_u64(candidates.len()),
                )
                .into(),
        );
    }
    read_candidates(candidates, package, limits)
}

fn package_archive_files(
    archive_path: &Path,
    sha256: &[u8; 32],
    language: PackageLanguage,
    package: &PackageIdentity,
    limits: &DependencyIndexLimits,
) -> Result<PackageFiles, PackageIndexError> {
    let archive = checked_package_archive(archive_path, sha256, package, limits)?;
    let archive_files = archive.into_files();
    let mut candidates = archive_files
        .into_iter()
        .filter(|(path, _)| archive_path_selected(path, language))
        .collect::<Vec<_>>();
    if language == PackageLanguage::Python {
        let has_stub = candidates.iter().any(|(path, _)| {
            !is_selected_documentation(path.as_str()) && is_python_stub(path.as_str())
        });
        if has_stub {
            candidates.retain(|(path, _)| {
                is_selected_documentation(path.as_str()) || is_python_stub(path.as_str())
            });
        }
    }
    if candidates.len() > limits.package_files_max {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::PackageFilesExceeded, package)
                .at(archive_path)
                .breached(
                    PACKAGE_FILES_MAX_FIELD,
                    count_u64(limits.package_files_max),
                    count_u64(candidates.len()),
                )
                .into(),
        );
    }
    let selected_bytes = candidates.iter().fold(0_u64, |total, (_, bytes)| {
        total.saturating_add(count_u64(bytes.len()))
    });
    if selected_bytes > limits.package_bytes_max {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::PackageBytesExceeded, package)
                .at(archive_path)
                .breached(
                    PACKAGE_BYTES_MAX_FIELD,
                    limits.package_bytes_max,
                    selected_bytes,
                )
                .into(),
        );
    }
    let mut files = Vec::with_capacity(candidates.len());
    let mut skipped_binary = 0_usize;
    for (path, bytes) in candidates {
        match text_content(bytes) {
            Some(content) => files.push(TextSourceFile::from_content(path, content)),
            None => skipped_binary = skipped_binary.saturating_add(1),
        }
    }
    Ok(PackageFiles::with_limits(
        files,
        skipped_binary,
        ExactPackageLimits::new(
            u32::try_from(limits.package_files_max).unwrap_or(u32::MAX),
            limits.package_bytes_max,
        ),
    ))
}

fn checked_package_archive(
    archive_path: &Path,
    sha256: &[u8; 32],
    package: &PackageIdentity,
    limits: &DependencyIndexLimits,
) -> Result<ArchiveFiles, PackageIndexError> {
    let bytes = read_archive_bytes(archive_path, package)?;
    let root = format!("{}-{}", package.name, package.version);
    let archive = read_archive(
        &bytes,
        ArchiveFormat::TarGzip,
        &ArchiveDigest::Sha256(*sha256),
        Some(&root),
        ArchiveLimits::default(),
    )
    .map_err(|error| {
        PackageIndexFault::new(PackageIndexViolation::Unreadable, package)
            .at(archive_path)
            .caused_by(error)
    })?;
    if archive.files().len() > limits.walk_entries_max {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::WalkEntriesExceeded, package)
                .at(archive_path)
                .breached(
                    WALK_ENTRIES_MAX_FIELD,
                    count_u64(limits.walk_entries_max),
                    count_u64(archive.files().len()),
                )
                .into(),
        );
    }
    for path in archive
        .files()
        .keys()
        .filter(|path| !path_has_skipped_directory(path))
    {
        let depth = path.as_str().split('/').count();
        if depth > limits.directory_depth_max.saturating_add(1) {
            return Err(PackageIndexFault::new(
                PackageIndexViolation::DirectoryDepthExceeded,
                package,
            )
            .at(archive_path)
            .breached(
                DIRECTORY_DEPTH_MAX_FIELD,
                count_u64(limits.directory_depth_max),
                count_u64(depth.saturating_sub(1)),
            )
            .into());
        }
    }
    Ok(archive)
}

fn read_archive_bytes(
    path: &Path,
    package: &PackageIdentity,
) -> Result<Vec<u8>, PackageIndexError> {
    let unreadable = |error: std::io::Error| {
        PackageIndexFault::new(PackageIndexViolation::Unreadable, package)
            .at(path)
            .caused_by(error)
    };
    let metadata = fs::symlink_metadata(path).map_err(unreadable)?;
    let maximum = ArchiveLimits::default().compressed_bytes_max();
    let length = metadata.len();
    if !metadata.is_file() || length > count_u64(maximum) {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::PackageBytesExceeded, package)
                .at(path)
                .breached("archive.compressed_bytes", count_u64(maximum), length)
                .into(),
        );
    }
    let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(maximum));
    fs::File::open(path)
        .map_err(unreadable)?
        .take(count_u64(maximum).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() > maximum {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::PackageBytesExceeded, package)
                .at(path)
                .breached(
                    "archive.compressed_bytes",
                    count_u64(maximum),
                    count_u64(bytes.len()),
                )
                .into(),
        );
    }
    Ok(bytes)
}

fn archive_path_selected(path: &ProjectPath, language: PackageLanguage) -> bool {
    !path_has_skipped_directory(path)
        && path
            .as_str()
            .rsplit('/')
            .next()
            .is_some_and(|name| language.is_candidate(name) || is_selected_documentation(name))
}

fn path_has_skipped_directory(path: &ProjectPath) -> bool {
    path.as_str()
        .split('/')
        .take(path.as_str().matches('/').count())
        .any(is_skipped_directory_name)
}

fn is_python_stub(path: &str) -> bool {
    path.ends_with(PYTHON_STUB_SUFFIX)
}

/// Every candidate file below `root`, or the root itself when it is one file.
fn candidates_below(
    root: &Path,
    language: PackageLanguage,
    package: &PackageIdentity,
    limits: &DependencyIndexLimits,
) -> Result<Vec<Candidate>, PackageIndexError> {
    let metadata = fs::metadata(root).map_err(|error| {
        PackageIndexFault::new(PackageIndexViolation::Unreadable, package)
            .at(root)
            .caused_by(error)
    })?;
    if metadata.is_file() {
        return Ok(
            single_file_candidate(root, metadata.len(), language, package)?
                .into_iter()
                .collect(),
        );
    }
    let mut candidates = Vec::new();
    let walked = bounded_for!(
        entry in package_walk(root, limits.directory_depth_max),
        budget = LoopBudget::new(limits.walk_entries_max),
        {
            let entry = entry.map_err(|error| {
                PackageIndexFault::new(PackageIndexViolation::Unreadable, package)
                    .at(root)
                    .caused_by(error)
            })?;
            if let Some(candidate) = candidate_of(&entry, root, language, package, limits)? {
                candidates.push(candidate);
            }
        }
    );
    walked.map_err(|exhausted| {
        PackageIndexFault::new(PackageIndexViolation::WalkEntriesExceeded, package)
            .at(root)
            .breached(
                WALK_ENTRIES_MAX_FIELD,
                count_u64(limits.walk_entries_max),
                count_u64(exhausted.limit().saturating_add(1)),
            )
    })?;
    Ok(candidates)
}

/// A single-file source root as its own candidate, when its name is selected.
fn single_file_candidate(
    root: &Path,
    bytes: u64,
    language: PackageLanguage,
    package: &PackageIdentity,
) -> Result<Option<Candidate>, PackageIndexError> {
    let Some(name) = root.file_name().and_then(OsStr::to_str) else {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::InvalidPath, package)
                .at(root)
                .into(),
        );
    };
    if !language.is_candidate(name) && !is_selected_documentation(name) {
        return Ok(None);
    }
    let relative = ProjectPath::new(name).map_err(|error| {
        PackageIndexFault::new(PackageIndexViolation::InvalidPath, package)
            .at(root)
            .caused_by(error)
    })?;
    Ok(Some(Candidate {
        absolute: root.to_path_buf(),
        relative,
        bytes,
    }))
}

/// One walked entry as a candidate: a selected file, or nothing for anything else.
fn candidate_of(
    entry: &DirEntry,
    root: &Path,
    language: PackageLanguage,
    package: &PackageIdentity,
    limits: &DependencyIndexLimits,
) -> Result<Option<Candidate>, PackageIndexError> {
    let Some(file_type) = entry.file_type() else {
        return Ok(None);
    };
    if file_type.is_dir() {
        return directory_within_depth(entry, package, limits).map(|()| None);
    }
    if !file_type.is_file() {
        return Ok(None);
    }
    let Some(name) = entry.file_name().to_str() else {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::InvalidPath, package)
                .at(entry.path())
                .into(),
        );
    };
    if !language.is_candidate(name) && !is_selected_documentation(name) {
        return Ok(None);
    }
    let relative = package_relative(root, entry.path(), package)?;
    let bytes = entry
        .metadata()
        .map_err(|error| {
            PackageIndexFault::new(PackageIndexViolation::Unreadable, package)
                .at(entry.path())
                .caused_by(error)
        })?
        .len();
    Ok(Some(Candidate {
        absolute: entry.path().to_path_buf(),
        relative,
        bytes,
    }))
}

/// Refuses a directory the walk reached past `directory_depth_max`.
fn directory_within_depth(
    entry: &DirEntry,
    package: &PackageIdentity,
    limits: &DependencyIndexLimits,
) -> Result<(), PackageIndexError> {
    if entry.depth() > limits.directory_depth_max {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::DirectoryDepthExceeded, package)
                .at(entry.path())
                .breached(
                    DIRECTORY_DEPTH_MAX_FIELD,
                    count_u64(limits.directory_depth_max),
                    count_u64(entry.depth()),
                )
                .into(),
        );
    }
    Ok(())
}

/// The package-relative address of `absolute`, which the walk found below `root`.
fn package_relative(
    root: &Path,
    absolute: &Path,
    package: &PackageIdentity,
) -> Result<ProjectPath, PackageIndexError> {
    let relative = absolute.strip_prefix(root).map_err(|error| {
        PackageIndexFault::new(PackageIndexViolation::InvalidPath, package)
            .at(absolute)
            .caused_by(error)
    })?;
    relative_path(relative).map_err(|error| {
        PackageIndexFault::new(PackageIndexViolation::InvalidPath, package)
            .at(absolute)
            .caused_by(error)
            .into()
    })
}

/// Reads every candidate under `package_bytes_max`, skipping binary content.
///
/// The bound is checked against the sizes the walk recorded before any file is read, so
/// a refusal names the package's whole selected size. The check inside the loop stays for
/// a file that grew between the walk and its read; that refusal names the file. The loop
/// is bounded by the caller's `package_files_max` refusal.
fn read_candidates(
    candidates: Vec<Candidate>,
    package: &PackageIdentity,
    limits: &DependencyIndexLimits,
) -> Result<PackageFiles, PackageIndexError> {
    let selected_bytes = candidates.iter().fold(0_u64, |total, candidate| {
        total.saturating_add(candidate.bytes)
    });
    if selected_bytes > limits.package_bytes_max {
        return Err(
            PackageIndexFault::new(PackageIndexViolation::PackageBytesExceeded, package)
                .breached(
                    PACKAGE_BYTES_MAX_FIELD,
                    limits.package_bytes_max,
                    selected_bytes,
                )
                .into(),
        );
    }
    let mut files = Vec::with_capacity(candidates.len());
    let mut skipped_binary = 0_usize;
    let mut byte_count = 0_u64;
    for candidate in candidates {
        let remaining = limits.package_bytes_max.saturating_sub(byte_count);
        let bytes = read_within(&candidate.absolute, remaining, package)?;
        byte_count = byte_count.saturating_add(count_u64(bytes.len()));
        if byte_count > limits.package_bytes_max {
            return Err(PackageIndexFault::new(
                PackageIndexViolation::PackageBytesExceeded,
                package,
            )
            .at(&candidate.absolute)
            .breached(
                PACKAGE_BYTES_MAX_FIELD,
                limits.package_bytes_max,
                byte_count,
            )
            .into());
        }
        match text_content(bytes) {
            Some(content) => files.push(TextSourceFile::from_content(candidate.relative, content)),
            None => skipped_binary += 1,
        }
    }
    Ok(PackageFiles::with_limits(
        files,
        skipped_binary,
        ExactPackageLimits::new(
            u32::try_from(limits.package_files_max).unwrap_or(u32::MAX),
            limits.package_bytes_max,
        ),
    ))
}

/// Reads at most `remaining + 1` bytes: an oversized file is counted, never held whole.
fn read_within(
    path: &Path,
    remaining: u64,
    package: &PackageIdentity,
) -> Result<Vec<u8>, PackageIndexError> {
    let unreadable = |error: std::io::Error| {
        PackageIndexFault::new(PackageIndexViolation::Unreadable, package)
            .at(path)
            .caused_by(error)
    };
    let handle = fs::File::open(path).map_err(unreadable)?;
    let mut bytes = Vec::new();
    handle
        .take(remaining.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    Ok(bytes)
}

/// The bytes as UTF-8 text; `None` for a NUL-bearing or non-UTF-8 file, binary here.
fn text_content(bytes: Vec<u8>) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// One depth-bounded walk below a package root, in file-name order.
/// It never follows a symlink or enters a skipped directory.
fn package_walk(root: &Path, directory_depth_max: usize) -> Walk {
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(false)
        .require_git(false)
        .follow_links(false)
        .max_depth(Some(directory_depth_max.saturating_add(1)))
        .sort_by_file_name(OsStr::cmp)
        .filter_entry(package_walk_includes);
    builder.build()
}

/// The root always; below it, nothing symlinked and no skipped directory name.
fn package_walk_includes(entry: &DirEntry) -> bool {
    let is_root = entry.depth() == 0;
    let skipped = entry
        .file_name()
        .to_str()
        .is_some_and(is_skipped_directory_name);
    is_root || (!entry.path_is_symlink() && !skipped)
}

/// Whether `name` names a directory the walk leaves out.
fn is_skipped_directory_name(name: &str) -> bool {
    SKIPPED_DIRECTORY_NAMES.contains(&name)
        || name.trim_matches('_').ends_with(TESTS_DIRECTORY_SUFFIX)
}

#[cfg(test)]
mod tests {
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use rift_core::{ErrorCode, ErrorName, Fault as _, ProjectPath};
    use rift_dependency::{CatalogEntry, CatalogSource, PackageLocation};
    use rift_syntax::ShippedLanguage;
    use sha2::{Digest as _, Sha256};

    use super::super::fixture::{
        identity, language, rooted, sorted_paths, tokio, violation_of, write,
    };
    use super::{
        Candidate, DependencyIndexLimits, PackageIndexViolation, package_files, read_candidates,
    };

    fn cargo_archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        for (path, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(u64::try_from(content.len()).expect("fixture length"));
            header.set_mode(0o644);
            header.set_cksum();
            archive
                .append_data(&mut header, format!("beacon-1.0.0/{path}"), *content)
                .expect("archive member");
        }
        archive
            .into_inner()
            .expect("tar archive")
            .finish()
            .expect("gzip archive")
    }

    #[test]
    fn test_cached_archive_and_installed_directory_select_same_files() {
        let members = [
            ("src/lib.rs", b"pub fn spawn() {}\n".as_slice()),
            ("README.md", b"# Beacon\n".as_slice()),
            ("tests/ignored.rs", b"fn ignored() {}\n".as_slice()),
        ];
        let archive_bytes = cargo_archive(&members);
        let archive_digest: [u8; 32] = Sha256::digest(&archive_bytes).into();
        let archive_directory = tempfile::tempdir().expect("archive directory");
        let archive_path = archive_directory.path().join("beacon-1.0.0.crate");
        std::fs::write(&archive_path, &archive_bytes).expect("archive bytes");
        let package = identity("cargo", "beacon", "1.0.0");
        let archived_entry =
            CatalogEntry::dependency(package.clone(), language(ShippedLanguage::Rust), None, true)
                .with_source_archive(archive_path, archive_digest);
        assert!(matches!(
            archived_entry.source(),
            Some(CatalogSource::Archive { .. })
        ));

        let installed_directory = tempfile::tempdir().expect("installed directory");
        for (path, contents) in members {
            write(installed_directory.path(), path, contents);
        }
        let installed_entry = rooted(package, ShippedLanguage::Rust, installed_directory.path());
        let limits = DependencyIndexLimits::default();
        let archived = package_files(&archived_entry, &limits).expect("cached archive");
        let installed = package_files(&installed_entry, &limits).expect("installed tree");

        assert_eq!(sorted_paths(&archived), sorted_paths(&installed));
        assert_eq!(
            archived
                .files()
                .iter()
                .map(|file| (file.path().as_str(), file.content()))
                .collect::<Vec<_>>(),
            installed
                .files()
                .iter()
                .map(|file| (file.path().as_str(), file.content()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_package_files_rust_selects_api_and_documentation_files_and_skips_test_directories() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/lib.rs", b"pub fn spawn() {}\n");
        write(root.path(), "src/net/mod.rs", b"pub struct Socket;\n");
        write(root.path(), "src/notes.md", b"# not source\n");
        write(root.path(), "tests/it.rs", b"fn it() {}\n");
        write(root.path(), "benches/bench.rs", b"fn bench() {}\n");
        write(root.path(), "examples/demo.rs", b"fn main() {}\n");
        write(root.path(), "target/debug/build.rs", b"fn main() {}\n");
        write(root.path(), "integration_tests/flow.rs", b"fn flow() {}\n");
        write(root.path(), "__tests__/spec.rs", b"fn spec() {}\n");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(
            sorted_paths(&files),
            ["src/lib.rs", "src/net/mod.rs", "src/notes.md"]
        );
        assert_eq!(files.file_count(), 3);
        assert_eq!(files.skipped_binary(), 0);
        assert_eq!(files.byte_count(), 18 + 19 + 13);
    }

    #[test]
    fn test_package_files_python_prefers_stubs_when_the_package_ships_any() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "six.py", b"def public(): ...\n");
        write(root.path(), "six.pyi", b"def public() -> None: ...\n");
        write(root.path(), "pkg/__init__.py", b"");
        write(root.path(), "README.md", b"# six\n");
        let entry = rooted(
            identity("uv", "six", "1.17.0"),
            ShippedLanguage::Python,
            root.path(),
        );

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(sorted_paths(&files), ["README.md", "six.pyi"]);
    }

    #[test]
    fn test_package_files_python_takes_modules_when_no_stub_exists() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "pkg/__init__.py", b"");
        write(root.path(), "pkg/core.py", b"def run(): ...\n");
        write(
            root.path(),
            "pkg/__pycache__/core.cpython-313.pyc",
            b"\x00\x01",
        );
        let entry = rooted(
            identity("uv", "pkg", "0.1.0"),
            ShippedLanguage::Python,
            root.path(),
        );

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(sorted_paths(&files), ["pkg/__init__.py", "pkg/core.py"]);
    }

    #[test]
    fn test_package_files_typescript_takes_only_declaration_files() {
        let root = tempfile::tempdir().expect("package root");
        write(
            root.path(),
            "index.d.ts",
            b"export declare function connect(): void;\n",
        );
        write(root.path(), "index.js", b"module.exports = {};\n");
        write(root.path(), "index.ts", b"export function connect() {}\n");
        write(
            root.path(),
            "lib/util.d.ts",
            b"export declare const version: string;\n",
        );
        write(root.path(), "node_modules/dep/index.d.ts", b"export {};\n");
        let entry = rooted(
            identity("npm", "client", "2.0.0"),
            ShippedLanguage::TypeScript,
            root.path(),
        );

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(sorted_paths(&files), ["index.d.ts", "lib/util.d.ts"]);
    }

    #[test]
    fn test_package_files_refuses_past_package_bytes_max_naming_the_package() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/a.rs", b"0123456789");
        write(root.path(), "src/b.rs", b"0123456789");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());
        let limits = DependencyIndexLimits {
            package_bytes_max: 15,
            ..DependencyIndexLimits::default()
        };

        let error = package_files(&entry, &limits).expect_err("bytes past the bound refuse");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::PackageBytesExceeded
        );
        assert_eq!(error.fault().package(), &tokio());
        assert!(error.to_string().contains("cargo/tokio@1.53.1"));
        let evidence = error.fault().limit_evidence().expect("limit evidence");
        assert_eq!(evidence.field, "package_bytes_max");
        assert_eq!(evidence.limit, 15);
        assert_eq!(
            evidence.required, 20,
            "the requirement is the package's whole selected size, summed from the walk"
        );
        assert!(
            error.fault().path().is_none(),
            "a package-wide bound names no single file"
        );
    }

    #[test]
    fn test_read_candidates_refuses_a_file_that_grew_after_the_walk_naming_it() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/a.rs", b"0123456789");
        let candidate = Candidate {
            absolute: root.path().join("src/a.rs"),
            relative: ProjectPath::new("src/a.rs").expect("valid path"),
            bytes: 0,
        };
        let limits = DependencyIndexLimits {
            package_bytes_max: 5,
            ..DependencyIndexLimits::default()
        };

        let error = read_candidates(vec![candidate], &tokio(), &limits)
            .expect_err("the grown file crosses the bound");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::PackageBytesExceeded
        );
        assert_eq!(
            error.fault().path(),
            Some(root.path().join("src/a.rs").as_path())
        );
        let evidence = error.fault().limit_evidence().expect("limit evidence");
        assert_eq!(
            evidence.required, 6,
            "the read stops one byte past the bound, counted up to the ceiling plus one"
        );
    }

    #[test]
    fn test_package_files_accepts_exactly_package_bytes_max() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/a.rs", b"0123456789");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());
        let limits = DependencyIndexLimits {
            package_bytes_max: 10,
            ..DependencyIndexLimits::default()
        };

        let files = package_files(&entry, &limits).expect("exactly the bound is accepted");

        assert_eq!(files.byte_count(), 10);
    }

    #[test]
    fn test_package_files_refuses_past_package_files_max_naming_the_package() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/a.rs", b"");
        write(root.path(), "src/b.rs", b"");
        write(root.path(), "src/c.rs", b"");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());
        let limits = DependencyIndexLimits {
            package_files_max: 2,
            ..DependencyIndexLimits::default()
        };

        let error = package_files(&entry, &limits).expect_err("files past the bound refuse");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::PackageFilesExceeded
        );
        assert!(error.to_string().contains("cargo/tokio@1.53.1"));
        let evidence = error.fault().limit_evidence().expect("limit evidence");
        assert_eq!((evidence.limit, evidence.required), (2, 3));
    }

    #[test]
    fn test_package_files_refuses_a_directory_past_directory_depth_max() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/deep/lib.rs", b"");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());
        let limits = DependencyIndexLimits {
            directory_depth_max: 1,
            ..DependencyIndexLimits::default()
        };

        let error = package_files(&entry, &limits).expect_err("a deeper directory refuses");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::DirectoryDepthExceeded
        );
        assert_eq!(
            error.fault().path(),
            Some(root.path().join("src/deep").as_path())
        );
        assert!(error.to_string().contains("cargo/tokio@1.53.1"));
    }

    #[test]
    fn test_package_files_refuses_past_walk_entries_max() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/a.rs", b"");
        write(root.path(), "src/b.rs", b"");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());
        let limits = DependencyIndexLimits {
            walk_entries_max: 2,
            ..DependencyIndexLimits::default()
        };

        let error = package_files(&entry, &limits).expect_err("entries past the bound refuse");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::WalkEntriesExceeded
        );
        assert!(error.to_string().contains("cargo/tokio@1.53.1"));
        let evidence = error.fault().limit_evidence().expect("limit evidence");
        assert_eq!((evidence.limit, evidence.required), (2, 3));
    }

    #[test]
    fn test_package_files_skips_binary_and_non_utf8_files_and_counts_them() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/lib.rs", b"pub fn spawn() {}\n");
        write(root.path(), "src/latin.rs", b"// caf\xe9\n");
        write(root.path(), "src/nul.rs", b"fn a() {}\x00");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(sorted_paths(&files), ["src/lib.rs"]);
        assert_eq!(files.skipped_binary(), 2);
    }

    #[test]
    fn test_package_files_single_file_root_is_one_entry() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "six.py", b"def public(): ...\n");
        let entry = rooted(
            identity("uv", "six", "1.17.0"),
            ShippedLanguage::Python,
            &root.path().join("six.py"),
        );

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(sorted_paths(&files), ["six.py"]);
    }

    #[test]
    fn test_package_files_single_file_root_outside_the_selection_is_empty() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "notes.txt", b"nothing\n");
        let entry = rooted(
            identity("uv", "notes", "0.0.1"),
            ShippedLanguage::Python,
            &root.path().join("notes.txt"),
        );

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert!(files.files().is_empty());
    }

    #[test]
    fn test_package_files_refuses_an_entry_without_a_source_root() {
        let entry = CatalogEntry::new(
            tokio(),
            PackageLocation::Dependency,
            language(ShippedLanguage::Rust),
        );

        let error = package_files(&entry, &DependencyIndexLimits::default()).expect_err("no root");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::SourceRootMissing
        );
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::ResourceNotFound));
        assert!(error.to_string().contains("cargo/tokio@1.53.1"));
    }

    #[test]
    fn test_package_files_refuses_an_absent_source_root() {
        let root = tempfile::tempdir().expect("package root");
        let entry = rooted(tokio(), ShippedLanguage::Rust, &root.path().join("gone"));

        let error =
            package_files(&entry, &DependencyIndexLimits::default()).expect_err("absent root");

        assert_eq!(violation_of(&error), PackageIndexViolation::Unreadable);
        assert!(std::error::Error::source(&error).is_some());
        assert!(error.to_string().contains("cargo/tokio@1.53.1"));
    }

    #[test]
    fn test_package_files_refuses_a_language_without_an_api_selection() {
        let root = tempfile::tempdir().expect("package root");
        let entry = rooted(
            identity("npm", "docs", "1.0.0"),
            ShippedLanguage::Markdown,
            root.path(),
        );

        let error = package_files(&entry, &DependencyIndexLimits::default())
            .expect_err("markdown has no API selection");

        assert_eq!(
            violation_of(&error),
            PackageIndexViolation::LanguageUnsupported
        );
        assert_eq!(
            error.name(),
            ErrorName::Wire(ErrorCode::CapabilityUnavailable)
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_package_files_skips_symlinked_files_and_directories() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/lib.rs", b"pub fn spawn() {}\n");
        write(root.path(), "elsewhere/other.rs", b"pub fn other() {}\n");
        std::os::unix::fs::symlink(
            root.path().join("elsewhere/other.rs"),
            root.path().join("src/link.rs"),
        )
        .expect("file symlink");
        std::os::unix::fs::symlink(
            root.path().join("elsewhere"),
            root.path().join("src/linked"),
        )
        .expect("directory symlink");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(
            sorted_paths(&files),
            ["elsewhere/other.rs", "src/lib.rs"],
            "the symlinked file and directory are left out; the real file stays"
        );
    }

    #[test]
    fn test_package_files_refuses_a_walked_path_that_is_no_project_path() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), ".rift/lib.rs", b"pub fn hidden() {}\n");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());

        let error = package_files(&entry, &DependencyIndexLimits::default())
            .expect_err("the rift state directory is no project path");

        assert_eq!(violation_of(&error), PackageIndexViolation::InvalidPath);
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::UnsupportedPath));
        assert_eq!(
            error.fault().path(),
            Some(root.path().join(".rift/lib.rs").as_path())
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn test_package_files_refuses_a_single_file_root_that_is_no_project_path() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "six\\.py", b"def public(): ...\n");
        let entry = rooted(
            identity("uv", "six", "1.17.0"),
            ShippedLanguage::Python,
            &root.path().join("six\\.py"),
        );

        let error = package_files(&entry, &DependencyIndexLimits::default())
            .expect_err("a backslash in the file name");

        assert_eq!(violation_of(&error), PackageIndexViolation::InvalidPath);
        assert_eq!(
            error.fault().path(),
            Some(root.path().join("six\\.py").as_path())
        );
        assert!(std::error::Error::source(&error).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn test_package_files_leaves_out_an_entry_that_is_neither_file_nor_directory() {
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/lib.rs", b"pub fn spawn() {}\n");
        let _socket =
            std::os::unix::net::UnixListener::bind(root.path().join("s.rs")).expect("socket file");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());

        let files = package_files(&entry, &DependencyIndexLimits::default()).expect("selected");

        assert_eq!(sorted_paths(&files), ["src/lib.rs"]);
    }

    /// APFS refuses a file name outside UTF-8 with `EILSEQ`, so Linux alone hosts the fixture.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_package_files_refuses_a_file_whose_name_is_not_utf8() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/lib.rs", b"pub fn spawn() {}\n");
        let odd = root
            .path()
            .join("src")
            .join(OsStr::from_bytes(b"caf\xe9.rs"));
        std::fs::write(&odd, b"pub fn odd() {}\n").expect("fixture file");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());

        let error = package_files(&entry, &DependencyIndexLimits::default())
            .expect_err("a file name outside UTF-8");

        assert_eq!(violation_of(&error), PackageIndexViolation::InvalidPath);
        assert_eq!(error.fault().path(), Some(odd.as_path()));
        assert!(std::error::Error::source(&error).is_none());
    }

    /// APFS refuses a file name outside UTF-8 with `EILSEQ`, so Linux alone hosts the fixture.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_package_files_refuses_a_single_file_root_whose_name_is_not_utf8() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;
        let directory = tempfile::tempdir().expect("package root");
        let root = directory.path().join(OsStr::from_bytes(b"caf\xe9.py"));
        std::fs::write(&root, b"def public(): ...\n").expect("fixture file");
        let entry = rooted(
            identity("uv", "six", "1.17.0"),
            ShippedLanguage::Python,
            &root,
        );

        let error = package_files(&entry, &DependencyIndexLimits::default())
            .expect_err("a root name outside UTF-8");

        assert_eq!(violation_of(&error), PackageIndexViolation::InvalidPath);
        assert_eq!(error.fault().path(), Some(root.as_path()));
        assert!(std::error::Error::source(&error).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn test_package_files_refuses_an_unreadable_directory_naming_the_root() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/lib.rs", b"pub fn spawn() {}\n");
        let locked = root.path().join("src");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("fixture permissions set");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());

        let error = package_files(&entry, &DependencyIndexLimits::default());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("fixture permissions restore");
        let error = error.expect_err("a directory this process cannot read");

        assert_eq!(violation_of(&error), PackageIndexViolation::Unreadable);
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::StorageFailure));
        assert_eq!(error.fault().path(), Some(root.path()));
        assert!(std::error::Error::source(&error).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn test_package_files_refuses_an_unreadable_file_naming_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().expect("package root");
        write(root.path(), "src/lib.rs", b"pub fn spawn() {}\n");
        let locked = root.path().join("src/lib.rs");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("fixture permissions set");
        let entry = rooted(tokio(), ShippedLanguage::Rust, root.path());

        let error = package_files(&entry, &DependencyIndexLimits::default());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644))
            .expect("fixture permissions restore");
        let error = error.expect_err("a file this process cannot read");

        assert_eq!(violation_of(&error), PackageIndexViolation::Unreadable);
        assert_eq!(error.name(), ErrorName::Wire(ErrorCode::StorageFailure));
        assert_eq!(error.fault().path(), Some(locked.as_path()));
        assert!(std::error::Error::source(&error).is_some());
    }
}
