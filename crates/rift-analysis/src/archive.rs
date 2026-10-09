//! Verified archive bytes shared by local cache and release adapters.
//!
//! This module reads bytes supplied by its caller. It writes no files and starts no processes.

use std::{
    cell::Cell,
    collections::BTreeMap,
    io::{Cursor, Read, Seek, SeekFrom},
};

use rift_core::{FileDigest, ProjectPath};
use sha2::{Digest, Sha256, Sha512};

/// Registry archive container.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveFormat {
    /// Gzip-compressed tar archive.
    TarGzip,
    /// ZIP archive with a supported compression method.
    Zip,
}

/// Expected digest obtained independently from exact release metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArchiveDigest {
    /// SHA-256 digest used by Cargo and `PyPI`.
    Sha256([u8; 32]),
    /// SHA-512 digest accepted from npm integrity metadata.
    Sha512([u8; 64]),
}

/// Work and allocation bounds for one archive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArchiveLimits {
    compressed_bytes: usize,
    expanded_bytes: usize,
    member_bytes: usize,
    extension_bytes: usize,
    members: usize,
    expansion_ratio: usize,
}

impl ArchiveLimits {
    /// Returns the compressed input byte bound.
    #[must_use]
    pub const fn compressed_bytes_max(self) -> usize {
        self.compressed_bytes
    }

    /// Replaces the byte bound for one GNU or PAX extended header.
    ///
    /// Extended headers also pass the member byte bound before their contents are allocated.
    ///
    /// # Errors
    /// Returns [`ArchiveError::InvalidLimits`] for zero or an unsupported byte bound.
    pub fn with_extension_size(mut self, extension_bytes: usize) -> Result<Self, ArchiveError> {
        let bytes = u64::try_from(extension_bytes).map_err(|_| ArchiveError::InvalidLimits)?;
        if !(1..=rift_protocol::configuration::ARCHIVE_EXTENSION_BYTES_MAX).contains(&bytes) {
            return Err(ArchiveError::InvalidLimits);
        }
        self.extension_bytes = extension_bytes;
        Ok(self)
    }

    /// Creates limits below the shared archive ceilings.
    ///
    /// # Errors
    /// Returns [`ArchiveError::InvalidLimits`] for zero, unordered, or excessive bounds.
    pub fn new(
        compressed_bytes: usize,
        expanded_bytes: usize,
        member_bytes: usize,
        members: usize,
        expansion_ratio: usize,
    ) -> Result<Self, ArchiveError> {
        use rift_protocol::configuration::{ArchiveConfiguration, ByteSize};

        Self::from_configuration(&ArchiveConfiguration {
            compressed_size: ByteSize::from_bytes(
                u64::try_from(compressed_bytes).map_err(|_| ArchiveError::InvalidLimits)?,
            ),
            expanded_size: ByteSize::from_bytes(
                u64::try_from(expanded_bytes).map_err(|_| ArchiveError::InvalidLimits)?,
            ),
            member_size: ByteSize::from_bytes(
                u64::try_from(member_bytes).map_err(|_| ArchiveError::InvalidLimits)?,
            ),
            extension_size: ByteSize::from_bytes(TAR_EXTENSION_BYTES_MAX),
            members: u32::try_from(members).map_err(|_| ArchiveError::InvalidLimits)?,
            expansion_ratio: u32::try_from(expansion_ratio)
                .map_err(|_| ArchiveError::InvalidLimits)?,
        })
    }

    /// Uses the archive acquisition bounds accepted from configuration and environment.
    ///
    /// # Errors
    /// Returns [`ArchiveError::InvalidLimits`] for zero, unordered, excessive,
    /// or unrepresentable bounds.
    pub fn from_configuration(
        configuration: &rift_protocol::configuration::ArchiveConfiguration,
    ) -> Result<Self, ArchiveError> {
        configuration
            .validate()
            .map_err(|_| ArchiveError::InvalidLimits)?;
        Ok(Self {
            compressed_bytes: usize::try_from(configuration.compressed_size.bytes())
                .map_err(|_| ArchiveError::InvalidLimits)?,
            expanded_bytes: usize::try_from(configuration.expanded_size.bytes())
                .map_err(|_| ArchiveError::InvalidLimits)?,
            member_bytes: usize::try_from(configuration.member_size.bytes())
                .map_err(|_| ArchiveError::InvalidLimits)?,
            extension_bytes: usize::try_from(configuration.extension_size.bytes())
                .map_err(|_| ArchiveError::InvalidLimits)?,
            members: usize::try_from(configuration.members)
                .map_err(|_| ArchiveError::InvalidLimits)?,
            expansion_ratio: usize::try_from(configuration.expansion_ratio)
                .map_err(|_| ArchiveError::InvalidLimits)?,
        })
    }
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            compressed_bytes: 64 * 1024 * 1024,
            expanded_bytes: 512 * 1024 * 1024,
            member_bytes: 64 * 1024 * 1024,
            extension_bytes: usize::try_from(TAR_EXTENSION_BYTES_MAX)
                .expect("default extension bytes fit usize"),
            members: 100_000,
            expansion_ratio: 200,
        }
    }
}

/// Complete regular files in canonical path order, after digest and archive validation.
#[derive(Debug)]
pub struct ArchiveFiles {
    digest: FileDigest,
    files: BTreeMap<ProjectPath, Vec<u8>>,
    skipped_links: Vec<ProjectPath>,
}

impl ArchiveFiles {
    /// Digest of the verified compressed archive bytes.
    #[must_use]
    pub const fn digest(&self) -> FileDigest {
        self.digest
    }

    /// Regular files after removing the caller's exact archive root, when supplied.
    #[must_use]
    pub fn files(&self) -> &BTreeMap<ProjectPath, Vec<u8>> {
        &self.files
    }

    /// Transfers the validated regular files to the source-selection adapter.
    #[must_use]
    pub fn into_files(self) -> BTreeMap<ProjectPath, Vec<u8>> {
        self.files
    }

    /// Paths of symbolic and hard link entries the archive held; never read, never followed.
    ///
    /// A skipped link counts toward `ArchiveLimits::members`, the same bound that limits
    /// files and directories; it has no separate bound.
    #[must_use]
    pub fn skipped_links(&self) -> &[ProjectPath] {
        &self.skipped_links
    }
}

/// Refusal while verifying or decoding an untrusted archive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveError {
    /// Limits exceed the supported ceilings.
    InvalidLimits,
    /// Compressed bytes exceed their bound.
    CompressedLimit,
    /// Archive bytes differ from the expected release digest.
    DigestMismatch,
    /// Container data, compression, CRC, or metadata is invalid.
    InvalidArchive,
    /// Decompressed bytes or expansion ratio exceed their bound.
    ExpandedLimit,
    /// Member count, size, or extended-header size exceeds its bound.
    MemberLimit,
    /// Archive metadata reads or seeks exceed their work bound.
    WorkLimit,
    /// A path is absolute, traverses parents, or is otherwise non-canonical.
    UnsafePath,
    /// Multiple members name the same normalized path.
    DuplicatePath,
    /// Archive contains a device, FIFO, encrypted, or sparse entry.
    UnsupportedEntry,
    /// Archive member lies outside the caller's exact release root.
    RootMismatch,
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidLimits => "archive limits are invalid",
            Self::CompressedLimit => "archive exceeds its compressed byte bound",
            Self::DigestMismatch => "archive digest differs from exact release metadata",
            Self::InvalidArchive => "archive data is invalid",
            Self::ExpandedLimit => "archive exceeds its decompressed byte or ratio bound",
            Self::MemberLimit => "archive exceeds its member bound",
            Self::WorkLimit => "archive metadata exceeds its work bound",
            Self::UnsafePath => "archive path is invalid",
            Self::DuplicatePath => "archive contains a duplicate path",
            Self::UnsupportedEntry => "archive entry type is unsupported",
            Self::RootMismatch => "archive member is outside its exact release root",
        })
    }
}

impl std::error::Error for ArchiveError {}

/// Verifies the complete archive before extracting regular files into memory.
///
/// `root` names an exact top-level directory from release metadata. `None` preserves paths.
/// Directories count toward the member bound but never become files. Extended tar headers count
/// toward both byte and member bounds before the tar library interprets them. A symbolic or
/// hard link entry is skipped rather than refused: it never becomes a file, its target is
/// never opened, and its path is carried in [`ArchiveFiles::skipped_links`].
/// Repeated regular files retain one file when their normalized paths and bytes match.
/// Repeated directories and links, conflicting bytes, and file-parent collisions are refused.
/// Every repeated member consumes the same member, decompressed-byte, and work bounds.
///
/// # Errors
/// Returns [`ArchiveError`] when the digest, paths, member types, container, or bounds fail.
pub fn read_archive(
    bytes: &[u8],
    format: ArchiveFormat,
    expected: &ArchiveDigest,
    root: Option<&str>,
    limits: ArchiveLimits,
) -> Result<ArchiveFiles, ArchiveError> {
    if bytes.is_empty() || bytes.len() > limits.compressed_bytes {
        return Err(ArchiveError::CompressedLimit);
    }
    let verified = match expected {
        ArchiveDigest::Sha256(expected) => <[u8; 32]>::from(Sha256::digest(bytes)) == *expected,
        ArchiveDigest::Sha512(expected) => <[u8; 64]>::from(Sha512::digest(bytes)) == *expected,
    };
    if !verified {
        return Err(ArchiveError::DigestMismatch);
    }
    if let Some(root) = root {
        let path = ProjectPath::new(root).map_err(|_| ArchiveError::UnsafePath)?;
        if path.as_str().is_empty() || root.contains('/') {
            return Err(ArchiveError::UnsafePath);
        }
    }
    let expanded_max = limits
        .expanded_bytes
        .min(bytes.len().saturating_mul(limits.expansion_ratio));
    let (files, skipped_links) = match format {
        ArchiveFormat::TarGzip => read_tar(bytes, root, limits, expanded_max)?,
        ArchiveFormat::Zip => read_zip(bytes, root, limits, expanded_max)?,
    };
    Ok(ArchiveFiles {
        digest: FileDigest::of(bytes),
        files,
        skipped_links,
    })
}

const TAR_EXTENSION_BYTES_MAX: u64 = rift_protocol::configuration::ARCHIVE_EXTENSION_BYTES_DEFAULT;

/// Regular files and skipped-link paths extracted from one archive container.
type ArchiveContents = (BTreeMap<ProjectPath, Vec<u8>>, Vec<ProjectPath>);

fn read_tar(
    bytes: &[u8],
    root: Option<&str>,
    limits: ArchiveLimits,
    expanded_max: usize,
) -> Result<ArchiveContents, ArchiveError> {
    let mut expanded = Vec::new();
    flate2::read::MultiGzDecoder::new(bytes)
        .take(u64::try_from(expanded_max).map_err(|_| ArchiveError::ExpandedLimit)? + 1)
        .read_to_end(&mut expanded)
        .map_err(|_| ArchiveError::InvalidArchive)?;
    if expanded.len() > expanded_max {
        return Err(ArchiveError::ExpandedLimit);
    }
    let mut raw = tar::Archive::new(expanded.as_slice());
    raw.set_ignore_zeros(true);
    // Raw iteration exposes GNU and PAX headers before the library allocates their contents.
    for (index, entry) in raw
        .entries()
        .map_err(|_| ArchiveError::InvalidArchive)?
        .raw(true)
        .enumerate()
    {
        if index >= limits.members {
            return Err(ArchiveError::MemberLimit);
        }
        let mut entry = entry.map_err(|_| ArchiveError::InvalidArchive)?;
        let kind = entry.header().entry_type();
        let extension = kind.is_gnu_longname() || kind.is_pax_local_extensions();
        let link = kind.is_symlink() || kind.is_hard_link();
        if !kind.is_file() && !kind.is_dir() && !extension && !link {
            return Err(ArchiveError::UnsupportedEntry);
        }
        if entry.size()
            > u64::try_from(limits.member_bytes).map_err(|_| ArchiveError::MemberLimit)?
            || (extension
                && entry.size()
                    > u64::try_from(limits.extension_bytes)
                        .map_err(|_| ArchiveError::MemberLimit)?)
        {
            return Err(ArchiveError::MemberLimit);
        }
        if let Some(values) = entry
            .pax_extensions()
            .map_err(|_| ArchiveError::InvalidArchive)?
        {
            for value in values {
                let value = value.map_err(|_| ArchiveError::InvalidArchive)?;
                if value.key_bytes().starts_with(b"GNU.sparse")
                    || value.key_bytes() == b"SCHILY.filetype"
                {
                    return Err(ArchiveError::UnsupportedEntry);
                }
            }
        }
    }
    let mut archive = tar::Archive::new(expanded.as_slice());
    archive.set_ignore_zeros(true);
    let mut output = ArchiveOutput::new(root, limits, expanded_max);
    for (index, entry) in archive
        .entries()
        .map_err(|_| ArchiveError::InvalidArchive)?
        .enumerate()
    {
        if index >= limits.members {
            return Err(ArchiveError::MemberLimit);
        }
        let mut entry = entry.map_err(|_| ArchiveError::InvalidArchive)?;
        let path = std::str::from_utf8(&entry.path_bytes())
            .map_err(|_| ArchiveError::UnsafePath)?
            .to_owned();
        let kind = entry.header().entry_type();
        if kind.is_symlink() || kind.is_hard_link() {
            output.push_skipped_link(&path)?;
            continue;
        }
        let directory = kind.is_dir();
        let size = entry.size();
        output.push(&path, directory, size, &mut entry)?;
    }
    Ok(output.into_files_and_skipped_links())
}

fn read_zip(
    bytes: &[u8],
    root: Option<&str>,
    limits: ArchiveLimits,
    expanded_max: usize,
) -> Result<ArchiveContents, ArchiveError> {
    let refused = Cell::new(None);
    let metadata = Cell::new(true);
    let declared_members = Cell::new(None);
    let reader = ZipReadGuard {
        cursor: Cursor::new(bytes),
        members: limits.members,
        refused: &refused,
        metadata: &metadata,
        declared_members: &declared_members,
        remaining_bytes: bytes.len().saturating_mul(4),
        remaining_operations: limits.members.saturating_mul(16).saturating_add(4096),
    };
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|_| refused.get().unwrap_or(ArchiveError::InvalidArchive))?;
    if let Some(error) = refused.get() {
        return Err(error);
    }
    metadata.set(false);
    if declared_members.get() != Some(archive.len() as u64) {
        // zip has already located any prepended bytes, including ZIP64 archives.
        let offset = usize::try_from(archive.offset()).map_err(|_| ArchiveError::InvalidArchive)?;
        let bytes = bytes.get(offset..).ok_or(ArchiveError::InvalidArchive)?;
        return read_repeated_zip(bytes, root, limits, expanded_max, archive.into_inner());
    }
    if archive.len() > limits.members {
        return Err(ArchiveError::MemberLimit);
    }
    let mut output = ArchiveOutput::new(root, limits, expanded_max);
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| zip_entry_error(&error))?;
        let directory = entry.is_dir();
        if zip_member_kind(
            directory,
            entry.is_symlink(),
            entry.unix_mode().unwrap_or(0),
        )? == ArchiveMemberKind::Link
        {
            let path = entry.name().to_owned();
            output.push_skipped_link(&path)?;
            continue;
        }
        let path = entry.name().to_owned();
        let size = entry.size();
        output.push(&path, directory, size, &mut entry)?;
    }
    Ok(output.into_files_and_skipped_links())
}

/// Reads every central-directory member when zip's name index collapsed repeated names.
///
/// The existing metadata guard has already bounded the archive. Each original record and
/// compressed payload also consumes its remaining work budget; retained files share tar's
/// path, member-type, byte-comparison, and decompressed-byte acceptance.
fn read_repeated_zip(
    bytes: &[u8],
    root: Option<&str>,
    limits: ArchiveLimits,
    expanded_max: usize,
    mut work: ZipReadGuard<'_>,
) -> Result<ArchiveContents, ArchiveError> {
    let archive =
        rawzip::ZipArchive::from_slice(bytes).map_err(|_| ArchiveError::InvalidArchive)?;
    let expected = work
        .declared_members
        .get()
        .ok_or(ArchiveError::InvalidArchive)?;
    if archive.entries_hint() != expected {
        return Err(ArchiveError::InvalidArchive);
    }
    let mut entries = archive.entries();
    let mut output = ArchiveOutput::new(root, limits, expanded_max);
    let mut count = 0_u64;
    while let Some(entry) = entries
        .next_entry()
        .map_err(|_| ArchiveError::InvalidArchive)?
    {
        if count >= limits.members as u64 {
            return Err(ArchiveError::MemberLimit);
        }
        count += 1;
        let (path, kind) = repeated_zip_member_metadata(bytes, &entry, &mut work)?;
        if entry.flags().is_encrypted() {
            return Err(ArchiveError::UnsupportedEntry);
        }
        let local = archive
            .get_entry(entry.wayfinder())
            .map_err(|_| ArchiveError::InvalidArchive)?;
        work.charge(local.data().len())?;
        push_repeated_zip_member(&mut output, &entry, &local, &path, kind)?;
    }
    if count != expected {
        return Err(ArchiveError::InvalidArchive);
    }
    Ok(output.into_files_and_skipped_links())
}

/// Fixed central-directory header bytes before its bounded name, extras, and comment.
const ZIP_CENTRAL_HEADER_BYTES: usize = 46;
/// Byte length of a ZIP32 end-of-central-directory record without a comment.
const ZIP_FOOTER_BYTES: usize = 22;
/// Central-directory byte-size field in a ZIP32 end-of-central-directory record.
const ZIP_FOOTER_DIRECTORY_SIZE: std::ops::Range<usize> = 12..16;
/// Local-header byte-offset field in a central-directory record.
const ZIP_CENTRAL_LOCAL_OFFSET: std::ops::Range<usize> = 42..46;

/// Reuses zip's name and member-type decoding for one original central record.
///
/// A writer supplies an empty stored local header and footer. The original central record
/// points to that header, so `by_index_raw` exposes CP437, UTF-8, Unicode-extra names and Unix
/// modes without decoding payloads. Its fixed header and three u16-length fields bound the
/// record to 196,651 bytes; generated local header and footer add 57 bytes.
fn repeated_zip_member_metadata(
    bytes: &[u8],
    entry: &rawzip::ZipFileHeaderRecord<'_>,
    work: &mut ZipReadGuard<'_>,
) -> Result<(String, ArchiveMemberKind), ArchiveError> {
    let length = ZIP_CENTRAL_HEADER_BYTES
        + entry.file_path().as_ref().len()
        + entry.extra_fields().remaining_bytes().len()
        + entry.comment().as_bytes().len();
    work.charge(length)?;
    let start = usize::try_from(entry.central_directory_offset())
        .map_err(|_| ArchiveError::InvalidArchive)?;
    let end = start
        .checked_add(length)
        .ok_or(ArchiveError::InvalidArchive)?;
    let record = bytes.get(start..end).ok_or(ArchiveError::InvalidArchive)?;
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    writer
        .start_file("entry", options)
        .map_err(|_| ArchiveError::InvalidArchive)?;
    let generated = writer
        .finish()
        .map_err(|_| ArchiveError::InvalidArchive)?
        .into_inner();
    let template =
        zip::ZipArchive::new(Cursor::new(&generated)).map_err(|_| ArchiveError::InvalidArchive)?;
    let local_bytes = usize::try_from(template.central_directory_start())
        .map_err(|_| ArchiveError::InvalidArchive)?;
    let mut footer = generated[generated.len() - ZIP_FOOTER_BYTES..].to_vec();
    footer[ZIP_FOOTER_DIRECTORY_SIZE].copy_from_slice(
        &u32::try_from(length)
            .map_err(|_| ArchiveError::InvalidArchive)?
            .to_le_bytes(),
    );
    let mut metadata = Vec::with_capacity(local_bytes + length + footer.len());
    metadata.extend_from_slice(&generated[..local_bytes]);
    metadata.extend_from_slice(record);
    metadata
        [local_bytes + ZIP_CENTRAL_LOCAL_OFFSET.start..local_bytes + ZIP_CENTRAL_LOCAL_OFFSET.end]
        .fill(0);
    metadata.extend_from_slice(&footer);
    let mut archive =
        zip::ZipArchive::new(Cursor::new(metadata)).map_err(|_| ArchiveError::InvalidArchive)?;
    let entry = archive
        .by_index_raw(0)
        .map_err(|_| ArchiveError::InvalidArchive)?;
    let kind = zip_member_kind(
        entry.is_dir(),
        entry.is_symlink(),
        entry.unix_mode().unwrap_or(0),
    )?;
    Ok((entry.name().to_owned(), kind))
}

fn push_repeated_zip_member(
    output: &mut ArchiveOutput<'_>,
    entry: &rawzip::ZipFileHeaderRecord<'_>,
    local: &rawzip::ZipSliceEntry<'_>,
    path: &str,
    kind: ArchiveMemberKind,
) -> Result<(), ArchiveError> {
    if ![
        rawzip::CompressionMethod::STORE,
        rawzip::CompressionMethod::DEFLATE,
    ]
    .contains(&entry.compression_method())
    {
        return Err(ArchiveError::InvalidArchive);
    }
    if kind == ArchiveMemberKind::Link {
        return output.push_skipped_link(path);
    }
    let directory = kind == ArchiveMemberKind::Directory;
    let size = entry.uncompressed_size_hint();
    match entry.compression_method() {
        rawzip::CompressionMethod::STORE => {
            let mut reader = local.verifying_reader(local.data());
            output.push(path, directory, size, &mut reader)
        }
        rawzip::CompressionMethod::DEFLATE => {
            let decoder = flate2::bufread::DeflateDecoder::new(local.data());
            let mut reader = local.verifying_reader(decoder);
            output.push(path, directory, size, &mut reader)
        }
        _ => Err(ArchiveError::InvalidArchive),
    }
}

fn zip_member_kind(
    directory: bool,
    symlink: bool,
    mode: u32,
) -> Result<ArchiveMemberKind, ArchiveError> {
    if symlink {
        return Ok(ArchiveMemberKind::Link);
    }
    let file_type = mode & 0o170_000;
    if ![0, 0o100_000, 0o040_000].contains(&file_type) || (file_type == 0o040_000 && !directory) {
        return Err(ArchiveError::UnsupportedEntry);
    }
    Ok(if directory {
        ArchiveMemberKind::Directory
    } else {
        ArchiveMemberKind::File
    })
}

/// zip 8.6 refuses to open an encrypted member without a password before the entry exists, so
/// the refusal itself is where an encrypted member is recognized.
fn zip_entry_error(error: &zip::result::ZipError) -> ArchiveError {
    match error {
        zip::result::ZipError::UnsupportedArchive(zip::result::ZipError::PASSWORD_REQUIRED) => {
            ArchiveError::UnsupportedEntry
        }
        _ => ArchiveError::InvalidArchive,
    }
}

// zip 8.6 allocates its central-directory Vec before exposing `len()`. Its fixed-size
// EOCD reads are 22 bytes (ZIP32) and 56 bytes (ZIP64), including the signature. Refuse
// declared counts at that read boundary, before the library can reserve member storage.
// Container interpretation and all field decoding remain in zip; this guard only bounds
// the count that controls its allocation. Cursor returns each fixed-size read in one call.
// Keep that count to reject duplicate raw names, which zip otherwise collapses in its index.
// Once metadata is decoded, file contents cannot be mistaken for a footer.
struct ZipReadGuard<'a> {
    cursor: Cursor<&'a [u8]>,
    members: usize,
    refused: &'a Cell<Option<ArchiveError>>,
    metadata: &'a Cell<bool>,
    declared_members: &'a Cell<Option<u64>>,
    remaining_bytes: usize,
    remaining_operations: usize,
}

impl ZipReadGuard<'_> {
    fn charge(&mut self, bytes: usize) -> Result<(), ArchiveError> {
        self.operation().map_err(|_| ArchiveError::WorkLimit)?;
        self.remaining_bytes = self
            .remaining_bytes
            .checked_sub(bytes)
            .ok_or(ArchiveError::WorkLimit)?;
        Ok(())
    }

    fn operation(&mut self) -> std::io::Result<()> {
        if self.remaining_operations == 0 || self.refused.get().is_some() {
            if self.refused.get().is_none() {
                self.refused.set(Some(ArchiveError::WorkLimit));
            }
            return Err(std::io::Error::other(
                "archive metadata exceeds its work bound",
            ));
        }
        self.remaining_operations -= 1;
        Ok(())
    }
}

impl Read for ZipReadGuard<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.operation()?;
        let length = self.cursor.read(buffer)?;
        if length > self.remaining_bytes {
            self.refused.set(Some(ArchiveError::WorkLimit));
            return Err(std::io::Error::other(
                "archive metadata exceeds its work bound",
            ));
        }
        self.remaining_bytes -= length;
        if !self.metadata.get() {
            return Ok(length);
        }
        let count = match (buffer.len(), length, buffer.get(..4)) {
            (22, 22, Some(b"PK\x05\x06")) => buffer
                .get(8..10)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u16::from_le_bytes)
                .map(u64::from),
            (56, 56, Some(b"PK\x06\x06")) => buffer
                .get(32..40)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u64::from_le_bytes),
            _ => None,
        };
        if count.is_some() {
            self.declared_members.set(count);
        }
        if count
            .is_some_and(|count| usize::try_from(count).map_or(true, |count| count > self.members))
        {
            self.refused.set(Some(ArchiveError::MemberLimit));
            return Err(std::io::Error::other(
                "archive member count exceeds its bound",
            ));
        }
        Ok(length)
    }
}

impl Seek for ZipReadGuard<'_> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.operation()?;
        self.cursor.seek(position)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArchiveMemberKind {
    File,
    Directory,
    Link,
}

struct ArchiveOutput<'a> {
    root: Option<&'a str>,
    limits: ArchiveLimits,
    remaining: usize,
    seen: BTreeMap<String, ArchiveMemberKind>,
    files: BTreeMap<ProjectPath, Vec<u8>>,
    skipped_links: Vec<ProjectPath>,
}

impl<'a> ArchiveOutput<'a> {
    fn new(root: Option<&'a str>, limits: ArchiveLimits, remaining: usize) -> Self {
        Self {
            root,
            limits,
            remaining,
            seen: BTreeMap::new(),
            files: BTreeMap::new(),
            skipped_links: Vec::new(),
        }
    }

    /// Normalizes an archive path, registers it against paths already seen, and returns the
    /// path relative to the caller's exact archive root. Shared by regular members and
    /// skipped links, so both draw duplicate-path and root-mismatch refusals from one place.
    fn accept_path(&mut self, path: &str, kind: ArchiveMemberKind) -> Result<String, ArchiveError> {
        let directory = kind == ArchiveMemberKind::Directory;
        if path.starts_with('/') || path.contains('\\') || path.split('/').any(|part| part == "..")
        {
            return Err(ArchiveError::UnsafePath);
        }
        let normalized = path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect::<Vec<_>>()
            .join("/");
        let full = ProjectPath::new(normalized).map_err(|_| ArchiveError::UnsafePath)?;
        let previous = self.seen.insert(full.as_str().to_owned(), kind);
        if previous.is_some_and(|previous| {
            previous != ArchiveMemberKind::File || kind != ArchiveMemberKind::File
        }) {
            return Err(ArchiveError::DuplicatePath);
        }
        for (separator, _) in full.as_str().match_indices('/') {
            if self
                .seen
                .get(&full.as_str()[..separator])
                .is_some_and(|kind| *kind != ArchiveMemberKind::Directory)
            {
                return Err(ArchiveError::DuplicatePath);
            }
        }
        let descendants = format!("{}/", full.as_str());
        if !directory
            && self
                .seen
                .range(descendants.clone()..)
                .next()
                .is_some_and(|(known, _)| known.starts_with(&descendants))
        {
            return Err(ArchiveError::DuplicatePath);
        }
        let relative = if let Some(root) = self.root {
            if full.as_str() == root && directory {
                String::new()
            } else {
                full.as_str()
                    .strip_prefix(root)
                    .and_then(|path| path.strip_prefix('/'))
                    .ok_or(ArchiveError::RootMismatch)?
                    .to_owned()
            }
        } else {
            full.as_str().to_owned()
        };
        Ok(relative)
    }

    fn push(
        &mut self,
        path: &str,
        directory: bool,
        size: u64,
        reader: &mut impl Read,
    ) -> Result<(), ArchiveError> {
        let kind = if directory {
            ArchiveMemberKind::Directory
        } else {
            ArchiveMemberKind::File
        };
        let relative = self.accept_path(path, kind)?;
        let size = usize::try_from(size).map_err(|_| ArchiveError::MemberLimit)?;
        if size > self.limits.member_bytes {
            return Err(ArchiveError::MemberLimit);
        }
        if size > self.remaining {
            return Err(ArchiveError::ExpandedLimit);
        }
        if directory {
            if size != 0 {
                return Err(ArchiveError::UnsupportedEntry);
            }
            return Ok(());
        }
        if relative.is_empty() {
            return Err(ArchiveError::UnsafePath);
        }
        let path = ProjectPath::new(relative).map_err(|_| ArchiveError::UnsafePath)?;
        let mut content = Vec::with_capacity(size.min(128 * 1024));
        reader
            .take(u64::try_from(size).map_err(|_| ArchiveError::MemberLimit)? + 1)
            .read_to_end(&mut content)
            .map_err(|_| ArchiveError::InvalidArchive)?;
        if content.len() != size {
            return Err(ArchiveError::InvalidArchive);
        }
        self.remaining -= size;
        match self.files.entry(path) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(content);
            }
            std::collections::btree_map::Entry::Occupied(entry) if entry.get() == &content => {}
            std::collections::btree_map::Entry::Occupied(_) => {
                return Err(ArchiveError::DuplicatePath);
            }
        }
        Ok(())
    }

    /// Registers a symbolic or hard link entry's path without reading its target or content.
    /// The link never becomes a file and its target is never opened.
    fn push_skipped_link(&mut self, path: &str) -> Result<(), ArchiveError> {
        let relative = self.accept_path(path, ArchiveMemberKind::Link)?;
        if relative.is_empty() {
            return Err(ArchiveError::UnsafePath);
        }
        let path = ProjectPath::new(relative).map_err(|_| ArchiveError::UnsafePath)?;
        self.skipped_links.push(path);
        Ok(())
    }

    fn into_files_and_skipped_links(self) -> ArchiveContents {
        (self.files, self.skipped_links)
    }
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;
