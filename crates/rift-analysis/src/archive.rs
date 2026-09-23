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
#[derive(Clone, Copy, Debug)]
pub struct ArchiveLimits {
    compressed_bytes: usize,
    expanded_bytes: usize,
    member_bytes: usize,
    members: usize,
    expansion_ratio: usize,
}

impl ArchiveLimits {
    /// Returns the compressed input byte bound.
    #[must_use]
    pub const fn compressed_bytes_max(self) -> usize {
        self.compressed_bytes
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
        let maximum = Self::default();
        if compressed_bytes == 0
            || compressed_bytes > maximum.compressed_bytes
            || expanded_bytes == 0
            || expanded_bytes > maximum.expanded_bytes
            || member_bytes == 0
            || member_bytes > expanded_bytes
            || member_bytes > maximum.member_bytes
            || members == 0
            || members > maximum.members
            || expansion_ratio == 0
            || expansion_ratio > maximum.expansion_ratio
        {
            return Err(ArchiveError::InvalidLimits);
        }
        Ok(Self {
            compressed_bytes,
            expanded_bytes,
            member_bytes,
            members,
            expansion_ratio,
        })
    }
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            compressed_bytes: 64 * 1024 * 1024,
            expanded_bytes: 512 * 1024 * 1024,
            member_bytes: 64 * 1024 * 1024,
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
    /// Archive contains a link, special file, encryption, or sparse file.
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
/// toward both byte and member bounds before the tar library interprets them.
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
    let files = match format {
        ArchiveFormat::TarGzip => read_tar(bytes, root, limits, expanded_max)?,
        ArchiveFormat::Zip => read_zip(bytes, root, limits, expanded_max)?,
    };
    Ok(ArchiveFiles {
        digest: FileDigest::of(bytes),
        files,
    })
}

const TAR_EXTENSION_BYTES_MAX: u64 = 16 * 1024;

fn read_tar(
    bytes: &[u8],
    root: Option<&str>,
    limits: ArchiveLimits,
    expanded_max: usize,
) -> Result<BTreeMap<ProjectPath, Vec<u8>>, ArchiveError> {
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
        if !kind.is_file() && !kind.is_dir() && !extension {
            return Err(ArchiveError::UnsupportedEntry);
        }
        if entry.size()
            > u64::try_from(limits.member_bytes).map_err(|_| ArchiveError::MemberLimit)?
            || (extension && entry.size() > TAR_EXTENSION_BYTES_MAX)
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
        let directory = entry.header().entry_type().is_dir();
        let size = entry.size();
        output.push(&path, directory, size, &mut entry)?;
    }
    Ok(output.files)
}

fn read_zip(
    bytes: &[u8],
    root: Option<&str>,
    limits: ArchiveLimits,
    expanded_max: usize,
) -> Result<BTreeMap<ProjectPath, Vec<u8>>, ArchiveError> {
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
        return Err(ArchiveError::DuplicatePath);
    }
    if archive.len() > limits.members {
        return Err(ArchiveError::MemberLimit);
    }
    let mut output = ArchiveOutput::new(root, limits, expanded_max);
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|_| ArchiveError::InvalidArchive)?;
        let directory = entry.is_dir();
        let file_type = entry.unix_mode().unwrap_or(0) & 0o170_000;
        if entry.encrypted()
            || entry.is_symlink()
            || ![0, 0o100_000, 0o040_000].contains(&file_type)
            || (file_type == 0o040_000 && !directory)
        {
            return Err(ArchiveError::UnsupportedEntry);
        }
        let path = entry.name().to_owned();
        let size = entry.size();
        output.push(&path, directory, size, &mut entry)?;
    }
    Ok(output.files)
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

struct ArchiveOutput<'a> {
    root: Option<&'a str>,
    limits: ArchiveLimits,
    remaining: usize,
    seen: BTreeMap<String, bool>,
    files: BTreeMap<ProjectPath, Vec<u8>>,
}

impl<'a> ArchiveOutput<'a> {
    fn new(root: Option<&'a str>, limits: ArchiveLimits, remaining: usize) -> Self {
        Self {
            root,
            limits,
            remaining,
            seen: BTreeMap::new(),
            files: BTreeMap::new(),
        }
    }

    fn push(
        &mut self,
        path: &str,
        directory: bool,
        size: u64,
        reader: &mut impl Read,
    ) -> Result<(), ArchiveError> {
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
        if self
            .seen
            .insert(full.as_str().to_owned(), directory)
            .is_some()
        {
            return Err(ArchiveError::DuplicatePath);
        }
        for (separator, _) in full.as_str().match_indices('/') {
            if self.seen.get(&full.as_str()[..separator]) == Some(&false) {
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
                ""
            } else {
                full.as_str()
                    .strip_prefix(root)
                    .and_then(|path| path.strip_prefix('/'))
                    .ok_or(ArchiveError::RootMismatch)?
            }
        } else {
            full.as_str()
        };
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
        self.files.insert(path, content);
        Ok(())
    }
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;
