//! Verified archive bytes shared by local cache and release adapters.
//!
//! This module reads bytes supplied by its caller. It writes no files and starts no processes.

use std::{
    borrow::Borrow,
    cell::{Cell, OnceCell},
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Cursor, ErrorKind, Read, Seek, SeekFrom},
};

use crate::input::ArchiveMemberKind;
use rift_core::{FileDigest, ProjectPath, constants::PROJECT_PATH_BYTES_MAX};
use rift_error::errors;
use sha2::{Digest, Sha256, Sha512};
use unicode_normalization::UnicodeNormalization;
use zstd::stream::raw::{self, Operation};

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

/// How the reader treats a regular file whose declared size exceeds the member byte bound.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OversizedMembers {
    /// Refuse the whole archive.
    #[default]
    Refuse,
    /// Read past the file without retaining its bytes and name it in
    /// [`ArchiveFiles::omissions`].
    ///
    /// The file still counts toward the member and decompressed byte bounds, and its decoded
    /// length must equal its declared size. Links, directories, and extended headers over the
    /// member byte bound are refused in both modes.
    Skip,
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
    oversized_members: OversizedMembers,
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

    /// Replaces how a regular file over the member byte bound is treated.
    #[must_use]
    pub const fn with_oversized_members(mut self, oversized_members: OversizedMembers) -> Self {
        self.oversized_members = oversized_members;
        self
    }

    /// Whether a regular file of this declared size is omitted rather than retained.
    fn omits(self, size: u64) -> bool {
        self.oversized_members == OversizedMembers::Skip && size > self.member_bytes as u64
    }

    /// The decompressed byte bound that applies to one archive of `compressed` bytes.
    fn expanded(self, compressed: usize) -> ExpandedBound {
        let ratio = compressed.saturating_mul(self.expansion_ratio);
        if ratio < self.expanded_bytes {
            ExpandedBound {
                bound: ArchiveBound::ExpansionRatio,
                maximum: ratio,
            }
        } else {
            ExpandedBound {
                bound: ArchiveBound::ExpandedBytes,
                maximum: self.expanded_bytes,
            }
        }
    }

    fn member_bytes_exceeded(self, observed: u64, member: Option<ProjectPath>) -> ArchiveError {
        ArchiveBound::MemberBytes.exceeded(observed, self.member_bytes as u64, member)
    }

    fn members_exceeded(self, observed: u64, member: Option<ProjectPath>) -> ArchiveError {
        ArchiveBound::Members.exceeded(observed, self.members as u64, member)
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
            oversized_members: OversizedMembers::default(),
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
            oversized_members: OversizedMembers::default(),
        }
    }
}

/// Archive path that follows every [`ProjectPath`] rule except Unicode NFC.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ArchivePath(String);

impl ArchivePath {
    /// The path in the archive's own Unicode spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Lets an ordered map keyed by archive path be probed by its text.
impl Borrow<str> for ArchivePath {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

/// Why the reader left one member out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveOmissionReason {
    /// The regular file exceeds the member byte bound under [`OversizedMembers::Skip`].
    Oversized,
    /// The name does not use Unicode NFC, so no [`ProjectPath`] names the member.
    NonCanonicalUnicode,
    /// The caller of [`read_archive_from`] did not select the file.
    Unselected,
}

/// A member the reader validated and left out.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveOmission {
    path: ArchivePath,
    kind: ArchiveMemberKind,
    reason: ArchiveOmissionReason,
    size: u64,
}

impl ArchiveOmission {
    /// Path after removing the caller's exact archive root, when supplied.
    #[must_use]
    pub const fn path(&self) -> &ArchivePath {
        &self.path
    }

    /// Entry kind of the member.
    #[must_use]
    pub const fn kind(&self) -> ArchiveMemberKind {
        self.kind
    }

    /// Why the member was left out.
    #[must_use]
    pub const fn reason(&self) -> ArchiveOmissionReason {
        self.reason
    }

    /// Declared size of a regular file in bytes, equal to its decoded length; zero for a
    /// directory or link.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }
}

/// Complete regular files in canonical path order, after digest and archive validation.
#[derive(Debug)]
pub struct ArchiveFiles {
    digest: FileDigest,
    files: BTreeMap<ProjectPath, Vec<u8>>,
    members: BTreeMap<String, ArchiveMemberKind>,
    skipped_links: Vec<ProjectPath>,
    omissions: Vec<ArchiveOmission>,
    replaced: BTreeSet<ArchivePath>,
}

impl ArchiveFiles {
    /// Digest of the verified compressed archive bytes.
    #[must_use]
    pub const fn digest(&self) -> FileDigest {
        self.digest
    }

    /// Admitted normalized member paths and entry kinds before source selection.
    /// Paths retain the original archive root even when regular files remove it. A member
    /// omitted as [`ArchiveOmissionReason::NonCanonicalUnicode`] is not admitted, so a caller
    /// that derives the archive root from these paths reads [`Self::omissions`] too.
    #[must_use]
    pub const fn members(&self) -> &BTreeMap<String, ArchiveMemberKind> {
        &self.members
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

    /// Members left out, in path order: files over the member byte bound under
    /// [`OversizedMembers::Skip`], files the caller did not select, and members whose name does
    /// not use Unicode NFC.
    ///
    /// Each path is absent from [`Self::files`]. An oversized or unselected path remains in
    /// [`Self::members`] as a file.
    /// The caller decides whether an omitted path leaves its source selection incomplete.
    #[must_use]
    pub fn omissions(&self) -> &[ArchiveOmission] {
        &self.omissions
    }

    /// Paths of regular files that an earlier member named with different bytes.
    ///
    /// The last member of a path is the one kept, or the one omitted.
    #[must_use]
    pub const fn replaced(&self) -> &BTreeSet<ArchivePath> {
        &self.replaced
    }
}

/// One quantity [`ArchiveLimits`] bounds while the reader decodes an archive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveBound {
    /// Members in the archive, including directories and links.
    Members,
    /// GNU and PAX extended headers in the archive, which are no members and share the member
    /// count's maximum.
    ExtendedHeaders,
    /// Bytes of one member.
    MemberBytes,
    /// Bytes of one GNU or PAX extended header.
    ExtensionBytes,
    /// Window bytes one Zstandard frame asks its decoder to allocate.
    WindowBytes,
    /// Decompressed bytes of the whole archive.
    ExpandedBytes,
    /// Decompressed bytes of the whole archive, bounded by its compressed bytes times the
    /// expansion ratio.
    ExpansionRatio,
}

impl ArchiveBound {
    const fn label(self) -> &'static str {
        match self {
            Self::Members => "member count",
            Self::ExtendedHeaders => "extended header count",
            Self::MemberBytes => "member byte",
            Self::ExtensionBytes => "extended header byte",
            Self::WindowBytes => "Zstandard window byte",
            Self::ExpandedBytes => "decompressed byte",
            Self::ExpansionRatio => "expansion ratio byte",
        }
    }

    fn exceeded(self, observed: u64, maximum: u64, member: Option<ProjectPath>) -> ArchiveError {
        ArchiveError::BoundExceeded(ArchiveBoundExceeded {
            bound: self,
            observed,
            maximum,
            member,
        })
    }
}

/// The bound an archive exceeded, with the observed quantity, the maximum, and the member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveBoundExceeded {
    bound: ArchiveBound,
    observed: u64,
    maximum: u64,
    member: Option<ProjectPath>,
}

impl ArchiveBoundExceeded {
    /// Bound the archive exceeded.
    #[must_use]
    pub const fn bound(&self) -> ArchiveBound {
        self.bound
    }

    /// Quantity that exceeded the bound.
    ///
    /// A declared quantity is reported whole. A quantity counted while decoding is reported
    /// where reading stopped, one past the bound, so the archive can hold more.
    #[must_use]
    pub const fn observed(&self) -> u64 {
        self.observed
    }

    /// Largest accepted quantity.
    #[must_use]
    pub const fn maximum(&self) -> u64 {
        self.maximum
    }

    /// Normalized path of the member that met the bound, with its original archive root.
    ///
    /// `None` when the bound was met before a member was decoded, or the member's path is not
    /// a valid project path. A refused extended header is named by its own path, such as
    /// `@LongLink`: the reader refuses it before it reads the member the header describes.
    #[must_use]
    pub const fn member(&self) -> Option<&ProjectPath> {
        self.member.as_ref()
    }
}

impl std::fmt::Display for ArchiveBoundExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "archive exceeds its {} bound: observed {}, maximum {}",
            self.bound.label(),
            self.observed,
            self.maximum
        )?;
        if let Some(member) = &self.member {
            write!(f, ", member {:?}", member.as_str())?;
        }
        Ok(())
    }
}

/// The path an archive was refused for: a member's own name, or the caller's archive root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsafeArchivePath {
    name: String,
    bytes_dropped: usize,
}

impl UnsafeArchivePath {
    fn new(name: &str) -> Self {
        let end = name.floor_char_boundary(PROJECT_PATH_BYTES_MAX);
        Self {
            name: name[..end].to_owned(),
            bytes_dropped: name.len() - end,
        }
    }

    /// The name as the archive or caller spells it, cut at the longest project path.
    ///
    /// Bytes that are not UTF-8 read as U+FFFD.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// UTF-8 bytes of the name past the cut.
    #[must_use]
    pub const fn bytes_dropped(&self) -> usize {
        self.bytes_dropped
    }
}

impl std::fmt::Display for UnsafeArchivePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "archive path is invalid: {:?}", self.name)?;
        if self.bytes_dropped > 0 {
            write!(f, " and {} more bytes", self.bytes_dropped)?;
        }
        Ok(())
    }
}

/// Refusal while verifying or decoding an untrusted archive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArchiveError {
    /// Limits exceed the supported ceilings.
    InvalidLimits,
    /// Compressed bytes exceed their bound.
    CompressedLimit,
    /// Archive bytes differ from the expected release digest.
    DigestMismatch,
    /// Container data, compression, CRC, or metadata is invalid.
    InvalidArchive,
    /// A member or decompressed quantity exceeds its bound.
    BoundExceeded(ArchiveBoundExceeded),
    /// Archive reads or seeks exceed their work bound.
    WorkLimit,
    /// A member name or the caller's archive root is absolute, traverses parents, or is
    /// otherwise no [`ArchivePath`].
    UnsafePath(UnsafeArchivePath),
    /// Members of one normalized path are not all regular files, or a file or link is the parent
    /// of another member.
    DuplicatePath,
    /// Archive contains a device, FIFO, encrypted, or sparse entry, or a PAX global header that
    /// changes how later members are read.
    UnsupportedEntry,
    /// Archive member lies outside the caller's exact release root.
    RootMismatch,
    /// The caller's reader failed.
    Read(ErrorKind),
    /// The caller's reader returned other bytes, or another count of them, than it did before.
    ReaderChanged,
}

impl ArchiveError {
    fn unsafe_path(name: &str) -> Self {
        Self::UnsafePath(UnsafeArchivePath::new(name))
    }
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::BoundExceeded(exceeded) => return std::fmt::Display::fmt(exceeded, f),
            Self::UnsafePath(path) => return std::fmt::Display::fmt(path, f),
            Self::Read(kind) => return write!(f, "archive reader failed: {kind}"),
            Self::InvalidLimits => "archive limits are invalid",
            Self::CompressedLimit => "archive exceeds its compressed byte bound",
            Self::DigestMismatch => "archive digest differs from exact release metadata",
            Self::InvalidArchive => "archive data is invalid",
            Self::WorkLimit => "archive reads exceed their work bound",
            Self::DuplicatePath => "archive contains a duplicate path",
            Self::UnsupportedEntry => "archive entry type is unsupported",
            Self::RootMismatch => "archive member is outside its exact release root",
            Self::ReaderChanged => "archive reader returned other bytes than before",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ArchiveError {}

/// Verifies the complete archive before extracting regular files into memory.
///
/// `root` names an exact top-level directory from release metadata. `None` preserves paths. The
/// directory that holds the root, `./`, lies outside no root.
/// Directories count toward the member bound but never become files. Extended tar headers are no
/// members: their own count has the member bound's maximum, and they count toward the byte bounds
/// before the tar library interprets them. A PAX global
/// header is metadata under no root: it is refused only with a record that changes a member's
/// path, link target, or size, or between another extended header and its member. A symbolic or
/// hard link entry is skipped rather than refused: it never becomes a file, its target is
/// never opened, and its path is carried in [`ArchiveFiles::skipped_links`].
/// The last regular file of one normalized path is kept, in archive order for tar and in central
/// directory order for ZIP, as tar extraction keeps it. [`ArchiveFiles::replaced`] lists the path
/// when an earlier file of it had different bytes. Repeated directories and links, a path that
/// changes its member type, and file-parent collisions are refused.
/// Every repeated member consumes the same member, decompressed-byte, and work bounds.
/// A regular file over the member byte bound refuses the archive unless the limits select
/// [`OversizedMembers::Skip`]. A member whose name does not use Unicode NFC is checked and
/// counted as any other and listed in [`ArchiveFiles::omissions`]. Collisions compare the
/// archive's own spelling, so two names that are equal only in Unicode NFC stay two members.
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
    read_archive_from(
        Cursor::new(bytes),
        format,
        expected,
        root,
        limits,
        &|_, _| true,
    )
}

/// Reads an archive as [`read_archive`] does, from a seekable reader, and retains the bytes of
/// the regular files `keep` selects.
///
/// `keep` answers for the path of a file, after the archive root is removed, and its declared
/// size. Every other file is checked, decoded, and counted as a kept one, its bytes are
/// dropped as they are decoded, and [`ArchiveFiles::omissions`] lists it as
/// [`ArchiveOmissionReason::Unselected`].
///
/// The reader is read from its start: once for the digest, and then once for tar and by member
/// for ZIP. It serves exactly the bytes whose digest was established: each block read
/// later is compared with the digest of the first pass before a byte of it is used. The reader
/// keeps buffers, the digest states of its blocks, the member inventory, and the selected files;
/// the container is never held whole.
///
/// # Errors
/// Returns [`ArchiveError`] as [`read_archive`] does, [`ArchiveError::Read`] when the reader
/// fails, [`ArchiveError::ReaderChanged`] when it serves other bytes than before, and
/// [`ArchiveError::WorkLimit`] when the container makes it read more blocks than its bound.
pub fn read_archive_from(
    mut reader: impl Read + Seek,
    format: ArchiveFormat,
    expected: &ArchiveDigest,
    root: Option<&str>,
    limits: ArchiveLimits,
    keep: &dyn Fn(&ProjectPath, u64) -> bool,
) -> Result<ArchiveFiles, ArchiveError> {
    if let Some(root) = root {
        let path = ProjectPath::new(root).map_err(|_| ArchiveError::unsafe_path(root))?;
        if path.as_str().is_empty() || root.contains('/') {
            return Err(ArchiveError::unsafe_path(root));
        }
    }
    let mut source = Source::verified(&mut reader, expected, limits)?;
    let result = read_source(&mut source, format, root, limits, keep);
    match source.failure {
        Some(error) => Err(error),
        None => result,
    }
}

fn read_source<R: Read + Seek>(
    source: &mut Source<'_, R>,
    format: ArchiveFormat,
    root: Option<&str>,
    limits: ArchiveLimits,
    keep: &dyn Fn(&ProjectPath, u64) -> bool,
) -> Result<ArchiveFiles, ArchiveError> {
    let compressed = usize::try_from(source.length).map_err(|_| ArchiveError::CompressedLimit)?;
    let mut output = ArchiveOutput::new(root, limits, limits.expanded(compressed));
    output.keep = keep;
    match format {
        ArchiveFormat::TarGzip => read_tar(source, compressed, &mut output)?,
        ArchiveFormat::Zip => read_zip(source, compressed, &mut output)?,
    }
    Ok(output.into_files(source.digest))
}

/// Bytes of one block of the caller's reader, read and compared at once.
const BLOCK_BYTES: usize = 64 * 1024;
/// Blocks held at once.
const BLOCKS_HELD: usize = 4;

/// Reads until `buffer` is full or the reader ends.
fn fill(reader: &mut impl Read, buffer: &mut [u8]) -> Result<usize, ArchiveError> {
    let mut filled = 0;
    while let Some(rest) = buffer.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        match reader.read(rest) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(ArchiveError::Read(error.kind())),
        }
    }
    Ok(filled)
}

/// The caller's reader, after one pass over it established its digest.
///
/// That pass keeps the state of the digest before every block. A block read later must lead
/// from its state to the next one, and every byte is served from such a block, so no byte is
/// used that the archive's digest does not cover. Comparing a block costs one digest of it, and
/// the states take about 112 bytes for each block of the compressed byte bound.
struct Source<'a, R> {
    reader: &'a mut R,
    digest: FileDigest,
    length: u64,
    /// The digest of the archive before each block, and after the last one.
    states: Vec<Sha256>,
    /// The blocks held, the latest first.
    held: Vec<(usize, Vec<u8>)>,
    position: u64,
    /// Block reads the container may still cost.
    reads: u64,
    failure: Option<ArchiveError>,
}

impl<'a, R: Read + Seek> Source<'a, R> {
    /// Reads the whole reader through one block and compares its digest with the expected one.
    /// The reader serves exactly the bytes it reports, so every later bound counts the bytes
    /// the digest covers.
    fn verified(
        reader: &'a mut R,
        expected: &ArchiveDigest,
        limits: ArchiveLimits,
    ) -> Result<Self, ArchiveError> {
        let failed = |error: std::io::Error| ArchiveError::Read(error.kind());
        let length = reader.seek(SeekFrom::End(0)).map_err(failed)?;
        if length == 0 || length > limits.compressed_bytes as u64 {
            return Err(ArchiveError::CompressedLimit);
        }
        reader.seek(SeekFrom::Start(0)).map_err(failed)?;
        let mut sha256 = Sha256::new();
        let mut sha512 = matches!(expected, ArchiveDigest::Sha512(_)).then(Sha512::new);
        let mut states = vec![Sha256::new()];
        let mut buffer = vec![0; BLOCK_BYTES];
        let mut served = 0_u64;
        loop {
            let read = fill(reader, &mut buffer)?;
            let Some(block) = buffer.get(..read).filter(|block| !block.is_empty()) else {
                break;
            };
            served += read as u64;
            if served > length {
                return Err(ArchiveError::ReaderChanged);
            }
            sha256.update(block);
            if let Some(sha512) = &mut sha512 {
                sha512.update(block);
            }
            states.push(sha256.clone());
        }
        if served != length {
            return Err(ArchiveError::ReaderChanged);
        }
        let actual = <[u8; 32]>::from(sha256.finalize());
        let verified = match expected {
            ArchiveDigest::Sha256(expected) => actual == *expected,
            ArchiveDigest::Sha512(expected) => {
                sha512.is_some_and(|sha512| <[u8; 64]>::from(sha512.finalize()) == *expected)
            }
        };
        if !verified {
            return Err(ArchiveError::DigestMismatch);
        }
        // One tar pass or every ZIP member reads each block, a ZIP directory is read again, and a member costs a few blocks of its own.
        let reads = (states.len() as u64 + limits.members as u64 + 1024).saturating_mul(4);
        Ok(Self {
            reader,
            digest: FileDigest::from_bytes(actual),
            length,
            states,
            held: Vec::with_capacity(BLOCKS_HELD),
            position: 0,
            reads,
            failure: None,
        })
    }

    fn restart(&mut self) {
        self.position = 0;
    }

    /// Moves block `index` to the front of the blocks held, reading and comparing it when it
    /// is not one of them.
    fn hold(&mut self, index: usize) -> Result<(), ArchiveError> {
        if let Some(held) = self.held.iter().position(|(block, _)| *block == index) {
            if let Some(held) = self.held.get_mut(..=held) {
                held.rotate_right(1);
            }
            return Ok(());
        }
        self.reads = self.reads.checked_sub(1).ok_or(ArchiveError::WorkLimit)?;
        let [before, after] = self.states.get(index..index + 2).unwrap_or_default() else {
            return Err(ArchiveError::ReaderChanged);
        };
        let start = index as u64 * BLOCK_BYTES as u64;
        let mut block = if self.held.len() < BLOCKS_HELD {
            Vec::new()
        } else {
            self.held.pop().map(|(_, block)| block).unwrap_or_default()
        };
        let length = BLOCK_BYTES.min(usize::try_from(self.length - start).unwrap_or(BLOCK_BYTES));
        block.resize(length, 0);
        self.reader
            .seek(SeekFrom::Start(start))
            .map_err(|error| ArchiveError::Read(error.kind()))?;
        let read = fill(self.reader, &mut block)?;
        let state = before.clone().chain_update(&block);
        if read != length || state.finalize() != after.clone().finalize() {
            return Err(ArchiveError::ReaderChanged);
        }
        self.held.insert(0, (index, block));
        Ok(())
    }
}

impl<R: Read + Seek> BufRead for Source<'_, R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.position >= self.length {
            return Ok(&[]);
        }
        let failed = || std::io::Error::other("archive reader failed");
        if self.failure.is_some() {
            return Err(failed());
        }
        let block = BLOCK_BYTES as u64;
        let index = usize::try_from(self.position / block).map_err(|_| failed())?;
        if let Err(error) = self.hold(index) {
            self.failure = Some(error);
            return Err(failed());
        }
        let offset = usize::try_from(self.position % block).map_err(|_| failed())?;
        let held = self.held.first().and_then(|(_, held)| held.get(offset..));
        held.ok_or_else(failed)
    }

    fn consume(&mut self, length: usize) {
        self.position += length as u64;
    }
}

impl<R: Read + Seek> Read for Source<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let mut bytes = self.fill_buf()?;
        let read = bytes.read(buffer)?;
        self.consume(read);
        Ok(read)
    }
}

impl<R> Seek for Source<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let position = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(offset) => self.length.checked_add_signed(offset),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
        };
        self.position = position.ok_or(ErrorKind::InvalidInput)?;
        Ok(self.position)
    }
}

const TAR_EXTENSION_BYTES_MAX: u64 = rift_protocol::configuration::ARCHIVE_EXTENSION_BYTES_DEFAULT;

/// Decodes the gzip stream once, checking every header and reading every member.
fn read_tar<R: Read + Seek>(
    source: &mut Source<'_, R>,
    compressed: usize,
    output: &mut ArchiveOutput<'_>,
) -> Result<(), ArchiveError> {
    source.restart();
    let mut stream = gzip_stream(source, output.expanded);
    let entries = read_tar_entries(&mut stream, output);
    // The rest of the stream decides the byte bound and gzip's own checks before a header or a
    // member refusal.
    std::io::copy(&mut stream, &mut std::io::sink()).map_err(|_| ArchiveError::InvalidArchive)?;
    check_expansion(&stream, compressed, output)?;
    entries?
}

/// The gzip stream of `source`, up to one byte past the decompressed byte bound.
fn gzip_stream<B: BufRead>(source: B, expanded: ExpandedBound) -> std::io::Take<GzipMembers<B>> {
    GzipMembers::new(source).take(expanded.maximum as u64 + 1)
}

/// Refuses a stream read to its end that decoded more than its bound. The gzip stream ends
/// where its padding starts, so padding earns no expansion.
fn check_expansion<B>(
    stream: &std::io::Take<GzipMembers<B>>,
    compressed: usize,
    output: &ArchiveOutput<'_>,
) -> Result<(), ArchiveError> {
    let decoded = output.expanded.maximum as u64 + 1 - stream.limit();
    if stream.limit() == 0 {
        return Err(output.expanded.exceeded(decoded, None));
    }
    let padding = usize::try_from(stream.get_ref().padding).unwrap_or(compressed);
    let bound = output.limits.expanded(compressed.saturating_sub(padding));
    if decoded > bound.maximum as u64 {
        return Err(bound.exceeded(bound.maximum as u64 + 1, None));
    }
    Ok(())
}

/// Decodes every gzip member and ends at the NUL bytes that pad the last one.
///
/// Other bytes after a member start another member, so anything else that trails the stream
/// is refused as an invalid member header.
struct GzipMembers<B> {
    /// The member being decoded; absent once the stream ended.
    member: Option<flate2::bufread::GzDecoder<B>>,
    /// NUL bytes after the last member.
    padding: u64,
}

impl<B: BufRead> GzipMembers<B> {
    fn new(source: B) -> Self {
        Self {
            member: Some(flate2::bufread::GzDecoder::new(source)),
            padding: 0,
        }
    }

    /// Reads past NUL padding to the end of `source`; whether another member starts instead.
    fn another_member(&mut self, source: &mut B) -> std::io::Result<bool> {
        loop {
            let bytes = source.fill_buf()?;
            let length = bytes.len();
            let padding = bytes.iter().take_while(|byte| **byte == 0).count();
            self.padding += padding as u64;
            if padding < length {
                return if self.padding == 0 {
                    Ok(true)
                } else {
                    Err(ErrorKind::InvalidData.into())
                };
            }
            if length == 0 {
                return Ok(false);
            }
            source.consume(length);
        }
    }
}

impl<B: BufRead> Read for GzipMembers<B> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        // Each pass consumes one member header, so the compressed bytes bound the loop.
        while let Some(member) = &mut self.member {
            let read = member.read(buffer)?;
            if read != 0 {
                return Ok(read);
            }
            let Some(member) = self.member.take() else {
                break;
            };
            let mut source = member.into_inner();
            if self.another_member(&mut source)? {
                self.member = Some(flate2::bufread::GzDecoder::new(source));
            }
        }
        Ok(0)
    }
}

/// Records that GNU and PAX extended headers hold for the next tar member.
#[derive(Default)]
struct TarExtension {
    /// Whether a PAX header of the next member was read.
    local: bool,
    long_name: Option<Vec<u8>>,
    /// Whether a GNU target header belongs to the next link.
    long_link: bool,
    path: Option<Vec<u8>>,
    size: Option<Vec<u8>>,
    /// Whether two PAX sizes name different quantities.
    sizes_differ: bool,
}

impl TarExtension {
    /// The member name as a tar reader resolves it: GNU long name, PAX path, header name.
    fn path_bytes<'a>(&'a self, header: &'a [u8]) -> &'a [u8] {
        let name = self
            .long_name
            .as_deref()
            .map(|name| name.strip_suffix(b"\0").unwrap_or(name));
        name.or(self.path.as_deref()).unwrap_or(header)
    }

    /// The member path that a refusal names, when it is one.
    fn member_path(&self, header: &[u8]) -> Option<ProjectPath> {
        let path = std::str::from_utf8(self.path_bytes(header)).ok()?;
        member_path(path).filter(|path| !path.as_str().is_empty())
    }

    /// Reads the records of one extended header, whose bytes the caller bounded.
    fn read(
        &mut self,
        kind: tar::EntryType,
        entry: &mut std::io::Take<impl Read>,
    ) -> Result<(), ArchiveError> {
        let global = kind.is_pax_global_extensions();
        if kind.is_gnu_longname() {
            let mut name = Vec::new();
            entry
                .read_to_end(&mut name)
                .map_err(|_| ArchiveError::InvalidArchive)?;
            self.long_name = Some(name);
            return Ok(());
        }
        if kind.is_gnu_longlink() {
            // The next link is skipped. The entry loop drains the bounded target bytes.
            self.long_link = true;
            return Ok(());
        }
        self.local |= !global;
        let mut payload =
            vec![0; usize::try_from(entry.limit()).map_err(|_| ArchiveError::InvalidArchive)?];
        entry
            .read_exact(&mut payload)
            .map_err(|_| ArchiveError::InvalidArchive)?;
        let mut records = payload.as_slice();
        while !records.is_empty() {
            let (key, value) = take_pax_record(&mut records)?;
            if is_sparse(key) || (global && changes_members(key)) {
                return Err(ArchiveError::UnsupportedEntry);
            }
            match key {
                // The first path record counts, as in the tar library.
                b"path" => {
                    self.path.get_or_insert_with(|| value.to_vec());
                }
                b"size" => self.record_size(value),
                _ => {}
            }
        }
        Ok(())
    }

    /// Whether an extended header of the next member was read.
    fn pending(&self) -> bool {
        self.local || self.long_name.is_some() || self.long_link
    }

    /// Keeps the first PAX size and whether another one differs from it.
    fn record_size(&mut self, size: &[u8]) {
        let first = self.size.get_or_insert_with(|| size.to_vec());
        self.sizes_differ |= first != size;
    }

    /// Whether any PAX size names another quantity than the member's own header.
    fn disagrees(&self, size: u64) -> bool {
        self.sizes_differ
            || self
                .size
                .as_deref()
                .is_some_and(|recorded| recorded != size.to_string().as_bytes())
    }
}

/// Reads the next PAX record. Its byte count includes the digits and final newline.
fn take_pax_record<'header>(
    bytes: &mut &'header [u8],
) -> Result<(&'header [u8], &'header [u8]), ArchiveError> {
    let space = bytes
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or(ArchiveError::InvalidArchive)?;
    let length = std::str::from_utf8(&bytes[..space])
        .ok()
        .and_then(|text| text.parse::<usize>().ok())
        .ok_or(ArchiveError::InvalidArchive)?;
    let (record, remaining) = bytes
        .split_at_checked(length)
        .ok_or(ArchiveError::InvalidArchive)?;
    let body = record
        .get(space + 1..)
        .and_then(|value| value.strip_suffix(b"\n"))
        .ok_or(ArchiveError::InvalidArchive)?;
    let equals = body
        .iter()
        .position(|byte| *byte == b'=')
        .ok_or(ArchiveError::InvalidArchive)?;
    *bytes = remaining;
    Ok((&body[..equals], &body[equals + 1..]))
}

/// Whether a PAX record names another path, link target, or size for a member.
///
/// A global header applies its records to every later member, which the tar library does
/// not do, so such a record would let another reader see a different archive.
fn changes_members(key: &[u8]) -> bool {
    [b"path".as_slice(), b"linkpath", b"size"].contains(&key)
}

fn is_sparse(key: &[u8]) -> bool {
    key.starts_with(b"GNU.sparse") || key == b"SCHILY.filetype"
}

const TAR_BLOCK_BYTES: u64 = 512;

/// Reads past zero blocks to the next complete header and verifies its checksum.
fn read_tar_header(reader: &mut impl Read) -> Result<Option<tar::Header>, ArchiveError> {
    let mut header = tar::Header::new_old();
    loop {
        let read = fill(reader, header.as_mut_bytes()).map_err(|_| ArchiveError::InvalidArchive)?;
        if read == 0 {
            return Ok(None);
        }
        if read != header.as_bytes().len() {
            return Err(ArchiveError::InvalidArchive);
        }
        if header.as_bytes().iter().all(|byte| *byte == 0) {
            continue;
        }
        let expected = header.cksum().map_err(|_| ArchiveError::InvalidArchive)?;
        let mut checked = header.clone();
        checked.set_cksum();
        if checked.cksum().ok() != Some(expected) {
            return Err(ArchiveError::InvalidArchive);
        }
        return Ok(Some(header));
    }
}

/// Reads every tar entry in one pass, and checks its header before its contents are read.
///
/// Reads raw headers so the archive bounds apply before allocating GNU and PAX extended data.
///
/// The outer error is the first header refusal. The inner one is the first member refusal: the
/// headers after it are still checked, and no member after it is read.
fn read_tar_entries(
    mut reader: impl Read,
    output: &mut ArchiveOutput<'_>,
) -> Result<Result<(), ArchiveError>, ArchiveError> {
    let limits = output.limits;
    let mut extended = TarExtension::default();
    let mut read = Ok(());
    let mut members = 0_u64;
    let mut extended_headers = 0_u64;
    while let Some(header) = read_tar_header(&mut reader)? {
        let size = header
            .entry_size()
            .map_err(|_| ArchiveError::InvalidArchive)?;
        let kind = header.entry_type();
        // In ustar a directory's size field records capacity. It has no data blocks.
        let size = if kind.is_dir() && header.as_ustar().is_some() {
            0
        } else {
            size
        };
        let extension = is_extended_header(&header);
        let (bound, count) = if extension {
            (ArchiveBound::ExtendedHeaders, &mut extended_headers)
        } else {
            (ArchiveBound::Members, &mut members)
        };
        *count += 1;
        if *count > limits.members as u64 {
            let member = extended.member_path(&header.path_bytes());
            return Err(bound.exceeded(*count, limits.members as u64, member));
        }
        let link = kind.is_symlink() || kind.is_hard_link();
        if !kind.is_file() && !kind.is_dir() && !extension && !link {
            return Err(ArchiveError::UnsupportedEntry);
        }
        // A global header between another extended header and its member names neither.
        if kind.is_pax_global_extensions() && extended.pending() {
            return Err(ArchiveError::UnsupportedEntry);
        }
        if (kind.is_pax_local_extensions() && extended.local)
            || (kind.is_gnu_longname() && extended.long_name.is_some())
            || (kind.is_gnu_longlink() && extended.long_link)
        {
            return Err(ArchiveError::InvalidArchive);
        }
        if size > limits.member_bytes as u64 && !(kind.is_file() && limits.omits(size)) {
            let member = extended.member_path(&header.path_bytes());
            return Err(limits.member_bytes_exceeded(size, member));
        }
        let mut entry = reader.by_ref().take(size);
        if extension {
            if size > limits.extension_bytes as u64 {
                return Err(ArchiveBound::ExtensionBytes.exceeded(
                    size,
                    limits.extension_bytes as u64,
                    extended.member_path(&header.path_bytes()),
                ));
            }
            extended.read(kind, &mut entry)?;
        } else {
            // A different PAX size would make another reader use different member boundaries.
            // A GNU long target header belongs to a link member.
            if extended.disagrees(size) || (extended.long_link && !link) {
                return Err(ArchiveError::InvalidArchive);
            }
            if read.is_ok() {
                read = push_tar_member(&extended, &header, &mut entry, output);
            }
            extended = TarExtension::default();
        }
        let padding = (TAR_BLOCK_BYTES - size % TAR_BLOCK_BYTES) % TAR_BLOCK_BYTES;
        let remaining = entry
            .limit()
            .checked_add(padding)
            .ok_or(ArchiveError::InvalidArchive)?;
        let mut unread = entry.into_inner().take(remaining);
        let discarded = std::io::copy(&mut unread, &mut std::io::sink())
            .map_err(|_| ArchiveError::InvalidArchive)?;
        if discarded != remaining {
            return Err(ArchiveError::InvalidArchive);
        }
    }
    if extended.pending() {
        read = read.and(Err(ArchiveError::InvalidArchive));
    }
    Ok(read)
}

/// Whether a header describes the next member or the archive rather than a member of its own.
///
/// The tar library reads GNU and PAX local records only of a header in the GNU or ustar form.
fn is_extended_header(header: &tar::Header) -> bool {
    let kind = header.entry_type();
    let recognized = header.as_gnu().is_some() || header.as_ustar().is_some();
    kind.is_pax_global_extensions()
        || (recognized
            && (kind.is_gnu_longname() || kind.is_gnu_longlink() || kind.is_pax_local_extensions()))
}

fn push_tar_member(
    extended: &TarExtension,
    header: &tar::Header,
    entry: &mut std::io::Take<impl Read>,
    output: &mut ArchiveOutput<'_>,
) -> Result<(), ArchiveError> {
    let path = extended.path_bytes(&header.path_bytes()).to_vec();
    let path = String::from_utf8(path)
        .map_err(|error| ArchiveError::unsafe_path(&String::from_utf8_lossy(error.as_bytes())))?;
    let kind = header.entry_type();
    if kind.is_symlink() || kind.is_hard_link() {
        return output.push_skipped_link(&path);
    }
    output.push(&path, kind.is_dir(), entry.limit(), entry)
}

fn read_zip<R: Read + Seek>(
    source: &mut Source<'_, R>,
    compressed: usize,
    output: &mut ArchiveOutput<'_>,
) -> Result<(), ArchiveError> {
    source.restart();
    let limits = output.limits;
    let refused = OnceCell::new();
    let metadata = Cell::new(true);
    let declared_members = Cell::new(None);
    let reader = ZipReadGuard {
        source,
        limits,
        refused: &refused,
        metadata: &metadata,
        declared_members: &declared_members,
        remaining_bytes: compressed.saturating_mul(4),
        remaining_operations: limits.members.saturating_mul(16).saturating_add(4096),
    };
    let opened = zip::ZipArchive::new(reader);
    if let Some(error) = refused.get() {
        return Err(error.clone());
    }
    let mut archive = opened.map_err(|_| ArchiveError::InvalidArchive)?;
    metadata.set(false);
    if declared_members.get() != Some(archive.len() as u64) {
        // zip has already located any prepended bytes, including ZIP64 archives.
        let start = archive.offset();
        let length = (compressed as u64)
            .checked_sub(start)
            .ok_or(ArchiveError::InvalidArchive)?;
        return read_repeated_zip((start, length), output, archive.into_inner());
    }
    if archive.len() > limits.members {
        return Err(limits.members_exceeded(archive.len() as u64, None));
    }
    for index in 0..archive.len() {
        // zip's own Zstandard decoder accepts libzstd's default window; Zstandard members are
        // read raw and decoded under the member byte bound instead.
        let zstd = archive
            .by_index_raw(index)
            .map_err(|error| zip_entry_error(&error))?
            .compression()
            == zip::CompressionMethod::Zstd;
        let mut entry = if zstd {
            archive.by_index_raw(index)
        } else {
            archive.by_index(index)
        }
        .map_err(|error| zip_entry_error(&error))?;
        if zstd && entry.encrypted() {
            return Err(ArchiveError::UnsupportedEntry);
        }
        let directory = entry.is_dir();
        if zip_member_kind(
            directory,
            entry.is_symlink(),
            entry.unix_mode().unwrap_or(0),
            entry.external_attributes(),
        )? == ArchiveMemberKind::Link
        {
            let path = entry
                .name()
                .map_err(|error| zip_entry_error(&error))?
                .into_owned();
            output.push_skipped_link(&path)?;
            continue;
        }
        let path = entry
            .name()
            .map_err(|error| zip_entry_error(&error))?
            .into_owned();
        let size = entry.size();
        if zstd {
            let expected = entry.crc32();
            let mut member = ZstdMember::new(BufReader::new(&mut entry), limits, size)?;
            let mut reader = Crc32Reader {
                inner: &mut member,
                crc: rawzip::Crc32::new(),
                expected,
            };
            output
                .push(&path, directory, size, &mut reader)
                .map_err(|error| member.refusal(error, &path))?;
        } else {
            output.push(&path, directory, size, &mut entry)?;
        }
    }
    Ok(())
}

/// Reads every central-directory member when zip's name index collapsed repeated names.
///
/// The existing metadata guard has already bounded the archive. Each original record and
/// compressed payload also consumes its remaining work budget; retained files share tar's
/// path, member-type, byte-comparison, and decompressed-byte acceptance.
fn read_repeated_zip<R: Read + Seek>(
    (start, length): (u64, u64),
    output: &mut ArchiveOutput<'_>,
    work: ZipReadGuard<'_, '_, R>,
) -> Result<(), ArchiveError> {
    let limits = output.limits;
    let (mut work, source) = work.into_work();
    let mut buffer = vec![0; ZIP_CENTRAL_RECORD_BYTES_MAX];
    let archive = rawzip::ZipArchive::from_seekable(Suffix { source, start }, &mut buffer)
        .map_err(|_| ArchiveError::InvalidArchive)?;
    let expected = work
        .declared_members
        .get()
        .ok_or(ArchiveError::InvalidArchive)?;
    if archive.entries_hint() != expected {
        return Err(ArchiveError::InvalidArchive);
    }
    let mut entries = archive.entries(&mut buffer);
    let mut count = 0_u64;
    while let Some(entry) = entries
        .next_entry()
        .map_err(|_| ArchiveError::InvalidArchive)?
    {
        if count >= limits.members as u64 {
            return Err(limits.members_exceeded(count + 1, None));
        }
        count += 1;
        let (path, kind) = repeated_zip_member_metadata(archive.get_ref(), &entry, &mut work)?;
        if entry.flags().is_encrypted() {
            return Err(ArchiveError::UnsupportedEntry);
        }
        let local = archive
            .get_entry(entry.wayfinder())
            .map_err(|_| ArchiveError::InvalidArchive)?;
        let (data_start, data_end) = local.compressed_data_range();
        if data_end > length {
            return Err(ArchiveError::InvalidArchive);
        }
        work.charge(data_end.saturating_sub(data_start))?;
        push_repeated_zip_member(output, &entry, &local, &path, kind)?;
    }
    if count != expected {
        return Err(ArchiveError::InvalidArchive);
    }
    Ok(())
}

/// The bytes of a reader from `start` on, as a reader of their own.
struct Suffix<S> {
    source: S,
    start: u64,
}

impl<S: Read> Read for Suffix<S> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.source.read(buffer)
    }
}

impl<S: Seek> Seek for Suffix<S> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let position = match position {
            SeekFrom::Start(offset) => SeekFrom::Start(
                offset
                    .checked_add(self.start)
                    .ok_or(ErrorKind::InvalidInput)?,
            ),
            relative => relative,
        };
        self.source
            .seek(position)?
            .checked_sub(self.start)
            .ok_or_else(|| ErrorKind::InvalidInput.into())
    }
}

/// Fixed central-directory header bytes before its bounded name, extras, and comment.
const ZIP_CENTRAL_HEADER_BYTES: usize = 46;
/// The longest central-directory record: its header, and three fields of u16 length.
const ZIP_CENTRAL_RECORD_BYTES_MAX: usize = ZIP_CENTRAL_HEADER_BYTES + 3 * 0xffff;
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
    source: &impl rawzip::ReaderAt,
    entry: &rawzip::ZipFileHeaderRecord<'_>,
    work: &mut ZipWork<'_>,
) -> Result<(String, ArchiveMemberKind), ArchiveError> {
    let length = ZIP_CENTRAL_HEADER_BYTES
        + entry.file_path().as_ref().len()
        + entry.extra_fields().remaining_bytes().len()
        + entry.comment().as_bytes().len();
    work.charge(length as u64)?;
    let mut record = vec![0; length];
    source
        .read_exact_at(&mut record, entry.central_directory_offset())
        .map_err(|_| ArchiveError::InvalidArchive)?;
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
    metadata.extend_from_slice(&record);
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
        entry.external_attributes(),
    )?;
    Ok((
        entry
            .name()
            .map_err(|error| zip_entry_error(&error))?
            .into_owned(),
        kind,
    ))
}

enum ZipCompression {
    Stored,
    Deflate,
    Zstd,
}

impl ZipCompression {
    fn from_method(method: rawzip::CompressionMethod) -> Result<Self, ArchiveError> {
        match method {
            rawzip::CompressionMethod::STORE => Ok(Self::Stored),
            rawzip::CompressionMethod::DEFLATE => Ok(Self::Deflate),
            rawzip::CompressionMethod::ZSTD => Ok(Self::Zstd),
            _ => Err(ArchiveError::InvalidArchive),
        }
    }
}

fn push_repeated_zip_member(
    output: &mut ArchiveOutput<'_>,
    entry: &rawzip::ZipFileHeaderRecord<'_>,
    local: &rawzip::ZipEntry<'_, impl rawzip::ReaderAt>,
    path: &str,
    kind: ArchiveMemberKind,
) -> Result<(), ArchiveError> {
    let compression = ZipCompression::from_method(entry.compression_method())?;
    if kind == ArchiveMemberKind::Link {
        return output.push_skipped_link(path);
    }
    let directory = kind == ArchiveMemberKind::Directory;
    let size = entry.uncompressed_size_hint();
    match compression {
        ZipCompression::Stored => {
            let mut reader = local.verifying_reader(local.reader());
            output.push(path, directory, size, &mut reader)
        }
        ZipCompression::Deflate => {
            let decoder = flate2::bufread::DeflateDecoder::new(BufReader::new(local.reader()));
            let mut reader = local.verifying_reader(decoder);
            output.push(path, directory, size, &mut reader)
        }
        ZipCompression::Zstd => {
            let source = BufReader::new(local.reader());
            let mut member = ZstdMember::new(source, output.limits, size)?;
            let mut reader = local.verifying_reader(&mut member);
            output
                .push(path, directory, size, &mut reader)
                .map_err(|error| member.refusal(error, path))
        }
    }
}

/// `ZSTD_WINDOWLOG_ABSOLUTEMIN`, the smallest window limit libzstd accepts.
const ZSTD_WINDOW_LOG_MIN: u32 = 10;
/// `ZSTD_WINDOWLOG_MAX_64`, the largest window limit libzstd accepts on 64-bit targets.
const ZSTD_WINDOW_LOG_MAX: u32 = 31;

/// The 8 MiB window RFC 8878 recommends every decoder support.
const ZSTD_WINDOW_LOG_OMITTED_MIN: u32 = 23;

/// Decodes one Zstandard member with a window no wider than the member byte bound.
///
/// Each frame header names the window its decoder allocates, and libzstd refuses a header
/// whose window exceeds `window_log_max` before allocating it. No retained member needs a
/// window wider than the member byte bound, so the decoder window stays below twice that bound.
/// An omitted member can need a wider one to have its length checked, so its window reaches
/// the recommended 8 MiB even when the member byte bound is narrower.
struct ZstdMember<R: BufRead> {
    reader: zstd::stream::zio::Reader<R, ZstdFrames>,
    window_log: u32,
    /// Window bytes of the frame libzstd refused.
    window_refused: Option<u64>,
}

impl<R: BufRead> ZstdMember<R> {
    fn new(reader: R, limits: ArchiveLimits, size: u64) -> Result<Self, ArchiveError> {
        let window_log_min = if limits.omits(size) {
            ZSTD_WINDOW_LOG_OMITTED_MIN
        } else {
            ZSTD_WINDOW_LOG_MIN
        };
        let window_log = limits
            .member_bytes
            .checked_next_power_of_two()
            .map_or(ZSTD_WINDOW_LOG_MAX, usize::trailing_zeros)
            .clamp(window_log_min, ZSTD_WINDOW_LOG_MAX);
        let frames = ZstdFrames::new(window_log).map_err(|_| ArchiveError::InvalidArchive)?;
        Ok(Self {
            reader: zstd::stream::zio::Reader::new(reader, frames),
            window_log,
            window_refused: None,
        })
    }

    /// Names a frame window over the decoder's as the bound it exceeds.
    fn refusal(&self, error: ArchiveError, path: &str) -> ArchiveError {
        let Some(observed) = self.window_refused else {
            return error;
        };
        let maximum = 1_u64 << self.window_log;
        ArchiveBound::WindowBytes.exceeded(observed, maximum, member_path(path))
    }
}

impl<R: BufRead> Read for ZstdMember<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.reader.read(buffer);
        if let Err(error) = &read
            && error.to_string() == zstd_window_refusal()
        {
            self.window_refused = self.reader.operation_mut().window();
        }
        read
    }
}

/// `ZSTD_FRAMEHEADERSIZE_MAX`, the bytes of the longest Zstandard frame header.
const ZSTD_FRAME_HEADER_BYTES_MAX: usize = 18;
/// Bytes of the magic number a frame header starts with.
const ZSTD_MAGIC_BYTES: usize = 4;
/// Frame header descriptor bit of a frame whose window is its content size.
const ZSTD_SINGLE_SEGMENT: u8 = 1 << 5;
/// Dictionary identifier bytes by the two lowest descriptor bits.
const ZSTD_DICTIONARY_BYTES: [usize; 4] = [0, 1, 2, 4];

/// Decodes Zstandard frames and retains the header of the current one.
///
/// libzstd refuses a window over `window_log_max` without reporting its width, so the width
/// is read from the header libzstd refused.
struct ZstdFrames {
    decoder: raw::Decoder<'static>,
    header: [u8; ZSTD_FRAME_HEADER_BYTES_MAX],
    retained: usize,
}

impl ZstdFrames {
    fn new(window_log: u32) -> std::io::Result<Self> {
        let mut decoder = raw::Decoder::new()?;
        decoder.set_parameter(raw::DParameter::WindowLogMax(window_log))?;
        Ok(Self {
            decoder,
            header: [0; ZSTD_FRAME_HEADER_BYTES_MAX],
            retained: 0,
        })
    }

    fn retain(&mut self, input: &[u8]) {
        for (byte, read) in self.header.iter_mut().skip(self.retained).zip(input) {
            *byte = *read;
            self.retained += 1;
        }
    }

    /// Window bytes the retained header asks for, in the layout RFC 8878 section 3.1.1.1
    /// defines.
    fn window(&self) -> Option<u64> {
        let header = self.header.get(ZSTD_MAGIC_BYTES..self.retained)?;
        let (descriptor, fields) = header.split_first()?;
        if descriptor & ZSTD_SINGLE_SEGMENT == 0 {
            let window = fields.first()?;
            let base = 1_u64 << (ZSTD_WINDOW_LOG_MIN + u32::from(window >> 3));
            return Some(base + (base >> 3) * u64::from(window & 7));
        }
        let dictionary = ZSTD_DICTIONARY_BYTES.get(usize::from(descriptor & 3))?;
        let size = fields.get(*dictionary..)?;
        let length = 1_usize << (descriptor >> 6);
        let mut content = [0; 8];
        content
            .get_mut(..length)?
            .copy_from_slice(size.get(..length)?);
        let content = u64::from_le_bytes(content);
        // A two-byte content size starts at 256.
        Some(if length == 2 { content + 256 } else { content })
    }
}

impl Operation for ZstdFrames {
    fn run<C: raw::WriteBuf + ?Sized>(
        &mut self,
        input: &mut raw::InBuffer<'_>,
        output: &mut raw::OutBuffer<'_, C>,
    ) -> std::io::Result<usize> {
        let start = input.pos();
        let hint = self.decoder.run(input, output);
        // A refused header stays unread, so the whole input can hold it.
        let end = if hint.is_ok() {
            input.pos()
        } else {
            input.src.len()
        };
        self.retain(input.src.get(start..end).unwrap_or_default());
        if matches!(hint, Ok(0)) {
            self.retained = 0;
        }
        hint
    }

    fn flush<C: raw::WriteBuf + ?Sized>(
        &mut self,
        output: &mut raw::OutBuffer<'_, C>,
    ) -> std::io::Result<usize> {
        self.decoder.flush(output)
    }

    fn reinit(&mut self) -> std::io::Result<()> {
        self.decoder.reinit()
    }

    fn finish<C: raw::WriteBuf + ?Sized>(
        &mut self,
        output: &mut raw::OutBuffer<'_, C>,
        finished_frame: bool,
    ) -> std::io::Result<usize> {
        self.decoder.finish(output, finished_frame)
    }
}

/// libzstd's name for a frame whose window exceeds the decoder's `window_log_max`.
///
/// The zstd crate reports a libzstd failure by this name alone. libzstd encodes error `code`
/// as `(size_t)-code`, the form its name lookup takes.
fn zstd_window_refusal() -> &'static str {
    let code = zstd::zstd_safe::zstd_sys::ZSTD_ErrorCode::ZSTD_error_frameParameter_windowTooLarge;
    zstd::zstd_safe::get_error_name(0_usize.wrapping_sub(code as usize))
}

/// Refuses a member whose decoded bytes end with a CRC-32 other than its recorded one, as
/// zip's own member reader does.
struct Crc32Reader<'a, R> {
    inner: &'a mut R,
    crc: rawzip::Crc32,
    expected: u32,
}

impl<R: Read> Read for Crc32Reader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read == 0 && self.crc.checksum() != self.expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ZIP member CRC-32 mismatch",
            ));
        }
        self.crc.update(buffer.get(..read).unwrap_or_default());
        Ok(read)
    }
}

fn zip_member_kind(
    directory: bool,
    symlink: bool,
    mode: u32,
    external_attributes: u32,
) -> Result<ArchiveMemberKind, ArchiveError> {
    // Nonzero high attributes contain Unix mode, including archives created on Windows.
    let unix_mode = external_attributes >> 16;
    let mode = if unix_mode == 0 { mode } else { unix_mode };
    if symlink || mode & 0o120_000 == 0o120_000 {
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

/// zip refuses to open an encrypted member without a password before the entry exists, so
/// the refusal itself is where an encrypted member is recognized.
fn zip_entry_error(error: &zip::result::ZipError) -> ArchiveError {
    match error {
        zip::result::ZipError::UnsupportedArchive(zip::result::ZipError::PASSWORD_REQUIRED) => {
            ArchiveError::UnsupportedEntry
        }
        _ => ArchiveError::InvalidArchive,
    }
}

// zip decodes central-directory members before exposing `len()`. Its fixed-size
// EOCD reads are 22 bytes (ZIP32) and 56 bytes (ZIP64), including the signature. Refuse
// declared counts at that read boundary, before the library can reserve member storage.
// Container interpretation and all field decoding remain in zip; this guard only bounds
// the count that controls its allocation. The guard fills each buffer, so a fixed-size read
// takes one call whatever the caller's reader returns.
// Keep that count to find duplicate raw names, which zip otherwise collapses in its index.
// Once metadata is decoded, file contents cannot be mistaken for a footer.
struct ZipReadGuard<'a, 'source, R> {
    source: &'a mut Source<'source, R>,
    limits: ArchiveLimits,
    refused: &'a OnceCell<ArchiveError>,
    metadata: &'a Cell<bool>,
    declared_members: &'a Cell<Option<u64>>,
    remaining_bytes: usize,
    remaining_operations: usize,
}

/// The work a ZIP archive may still cost, without the reader it was charged to.
struct ZipWork<'a> {
    refused: &'a OnceCell<ArchiveError>,
    declared_members: &'a Cell<Option<u64>>,
    remaining_bytes: usize,
    remaining_operations: usize,
}

impl ZipWork<'_> {
    fn charge(&mut self, bytes: u64) -> Result<(), ArchiveError> {
        zip_operation(self.refused, &mut self.remaining_operations)
            .map_err(|_| ArchiveError::WorkLimit)?;
        self.remaining_bytes = usize::try_from(bytes)
            .ok()
            .and_then(|bytes| self.remaining_bytes.checked_sub(bytes))
            .ok_or(ArchiveError::WorkLimit)?;
        Ok(())
    }
}

/// Consumes one operation of the ZIP work bound.
fn zip_operation(
    refused: &OnceCell<ArchiveError>,
    remaining_operations: &mut usize,
) -> std::io::Result<()> {
    if *remaining_operations == 0 || refused.get().is_some() {
        refused.get_or_init(|| ArchiveError::WorkLimit);
        return Err(std::io::Error::other(
            "archive metadata exceeds its work bound",
        ));
    }
    *remaining_operations -= 1;
    Ok(())
}

impl<'a, 'source, R: Read + Seek> ZipReadGuard<'a, 'source, R> {
    fn into_work(self) -> (ZipWork<'a>, &'a mut Source<'source, R>) {
        let work = ZipWork {
            refused: self.refused,
            declared_members: self.declared_members,
            remaining_bytes: self.remaining_bytes,
            remaining_operations: self.remaining_operations,
        };
        (work, self.source)
    }

    /// Reads until `buffer` is full or the archive ends.
    fn fill(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let mut filled = 0;
        while let Some(rest) = buffer.get_mut(filled..).filter(|rest| !rest.is_empty()) {
            match self.source.read(rest)? {
                0 => break,
                read => filled += read,
            }
        }
        Ok(filled)
    }
}

impl<R: Read + Seek> Read for ZipReadGuard<'_, '_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        zip_operation(self.refused, &mut self.remaining_operations)?;
        let length = self.fill(buffer)?;
        if length > self.remaining_bytes {
            self.refused.get_or_init(|| ArchiveError::WorkLimit);
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
        if let Some(count) = count.filter(|count| *count > self.limits.members as u64) {
            self.refused
                .get_or_init(|| self.limits.members_exceeded(count, None));
            return Err(std::io::Error::other(
                "archive member count exceeds its bound",
            ));
        }
        Ok(length)
    }
}

impl<R: Read + Seek> Seek for ZipReadGuard<'_, '_, R> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        zip_operation(self.refused, &mut self.remaining_operations)?;
        self.source.seek(position)
    }
}

/// The decompressed byte bound of one archive: its byte limit or its expansion ratio.
#[derive(Clone, Copy)]
struct ExpandedBound {
    bound: ArchiveBound,
    maximum: usize,
}

impl ExpandedBound {
    fn exceeded(self, observed: u64, member: Option<ProjectPath>) -> ArchiveError {
        self.bound.exceeded(observed, self.maximum as u64, member)
    }
}

/// A path that follows every project path rule, or every one except Unicode NFC.
enum AcceptedPath {
    Canonical(ProjectPath),
    NonCanonicalUnicode(ArchivePath),
}

impl AcceptedPath {
    /// The NFC form follows every project path rule too, so a name never passes a rule by
    /// its Unicode spelling alone.
    fn new(path: String) -> Option<Self> {
        match ProjectPath::new(path.as_str()) {
            Ok(path) => Some(Self::Canonical(path)),
            Err(error) if error.slug() == errors::core::path_non_canonical_unicode::SLUG => {
                ProjectPath::new(path.nfc().collect::<String>()).ok()?;
                Some(Self::NonCanonicalUnicode(ArchivePath(path)))
            }
            Err(_) => None,
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Canonical(path) => path.as_str(),
            Self::NonCanonicalUnicode(path) => path.as_str(),
        }
    }
}

/// Normalizes the separators of one untrusted archive path, keeping its archive root.
fn archive_path(path: &str) -> Result<AcceptedPath, ArchiveError> {
    if path.starts_with('/') || path.contains('\\') || path.split('/').any(|part| part == "..") {
        return Err(ArchiveError::unsafe_path(path));
    }
    let normalized = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect::<Vec<_>>()
        .join("/");
    AcceptedPath::new(normalized).ok_or_else(|| ArchiveError::unsafe_path(path))
}

/// The project path that names one member, keeping its archive root.
fn member_path(path: &str) -> Option<ProjectPath> {
    match archive_path(path) {
        Ok(AcceptedPath::Canonical(path)) => Some(path),
        Ok(AcceptedPath::NonCanonicalUnicode(_)) | Err(_) => None,
    }
}

/// One member's path below the archive root.
enum RelativePath {
    /// NFC text the root was removed from, not yet checked as a project path.
    Canonical(String),
    NonCanonicalUnicode(ArchivePath),
}

impl RelativePath {
    /// The project path that names a file or link, `None` for the archive root itself or a
    /// path that is unsafe without its root.
    fn project_path(relative: String) -> Option<ProjectPath> {
        ProjectPath::new(relative)
            .ok()
            .filter(|path| !path.as_str().is_empty())
    }
}

struct ArchiveOutput<'a> {
    root: Option<&'a str>,
    /// Whether the bytes of a file are retained, by its path below the root and its size.
    keep: &'a dyn Fn(&ProjectPath, u64) -> bool,
    limits: ArchiveLimits,
    expanded: ExpandedBound,
    remaining: usize,
    seen: BTreeMap<String, ArchiveMemberKind>,
    files: BTreeMap<ProjectPath, Vec<u8>>,
    skipped_links: Vec<ProjectPath>,
    omitted: BTreeMap<ArchivePath, OmittedContent>,
    replaced: BTreeSet<ArchivePath>,
}

/// One omitted member; the length and digest of a file are compared when its path repeats.
#[derive(Eq, PartialEq)]
struct OmittedContent {
    kind: ArchiveMemberKind,
    reason: ArchiveOmissionReason,
    size: u64,
    digest: [u8; 32],
}

/// Digests written bytes without retaining them.
#[derive(Default)]
struct DigestWriter(Sha256);

impl std::io::Write for DigestWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> ArchiveOutput<'a> {
    fn new(root: Option<&'a str>, limits: ArchiveLimits, expanded: ExpandedBound) -> Self {
        Self {
            root,
            keep: &|_, _| true,
            limits,
            expanded,
            remaining: expanded.maximum,
            seen: BTreeMap::new(),
            files: BTreeMap::new(),
            skipped_links: Vec::new(),
            omitted: BTreeMap::new(),
            replaced: BTreeSet::new(),
        }
    }

    /// Normalizes an archive path, registers it against paths already seen, and returns the
    /// path relative to the caller's exact archive root. Shared by regular members and
    /// skipped links, so both draw duplicate-path and root-mismatch refusals from one place.
    fn accept_path(
        &mut self,
        path: &str,
        kind: ArchiveMemberKind,
    ) -> Result<RelativePath, ArchiveError> {
        let directory = kind == ArchiveMemberKind::Directory;
        let full = archive_path(path)?;
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
            // `./` is the folder that holds the root.
            if directory && ["", root].contains(&full.as_str()) {
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
        match full {
            AcceptedPath::Canonical(_) => Ok(RelativePath::Canonical(relative)),
            AcceptedPath::NonCanonicalUnicode(_) => match AcceptedPath::new(relative) {
                Some(AcceptedPath::NonCanonicalUnicode(path)) => {
                    Ok(RelativePath::NonCanonicalUnicode(path))
                }
                Some(AcceptedPath::Canonical(_)) | None => Err(ArchiveError::unsafe_path(path)),
            },
        }
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
        let oversized = !directory && self.limits.omits(size);
        if size > self.limits.member_bytes as u64 && !oversized {
            return Err(self.limits.member_bytes_exceeded(size, member_path(path)));
        }
        let length = usize::try_from(size)
            .ok()
            .filter(|length| *length <= self.remaining)
            .ok_or_else(|| {
                let read = (self.expanded.maximum - self.remaining) as u64;
                self.expanded
                    .exceeded(read.saturating_add(size), member_path(path))
            })?;
        if directory {
            if size != 0 {
                return Err(ArchiveError::UnsupportedEntry);
            }
            if let RelativePath::NonCanonicalUnicode(path) = relative {
                self.omit_empty(path, kind);
            }
            return Ok(());
        }
        match relative {
            RelativePath::NonCanonicalUnicode(path) => {
                let reason = ArchiveOmissionReason::NonCanonicalUnicode;
                self.omit(path, reason, size, reader)?;
            }
            RelativePath::Canonical(relative) => {
                let path = RelativePath::project_path(relative)
                    .ok_or_else(|| ArchiveError::unsafe_path(path))?;
                let omission = if oversized {
                    Some(ArchiveOmissionReason::Oversized)
                } else {
                    (!(self.keep)(&path, size)).then_some(ArchiveOmissionReason::Unselected)
                };
                match omission {
                    Some(reason) => {
                        let path = ArchivePath(path.as_str().to_owned());
                        self.omit(path, reason, size, reader)?;
                    }
                    None => self.retain(path, size, length, reader)?,
                }
            }
        }
        self.remaining -= length;
        Ok(())
    }

    /// Retains one regular file's bytes in place of an earlier file of its path.
    fn retain(
        &mut self,
        path: ProjectPath,
        size: u64,
        length: usize,
        reader: &mut impl Read,
    ) -> Result<(), ArchiveError> {
        let mut content = Vec::with_capacity(length.min(128 * 1024));
        reader
            .take(size + 1)
            .read_to_end(&mut content)
            .map_err(|_| ArchiveError::InvalidArchive)?;
        if content.len() != length {
            return Err(ArchiveError::InvalidArchive);
        }
        let replaced = self.omitted.remove(path.as_str()).is_some()
            || self
                .files
                .get(&path)
                .is_some_and(|earlier| *earlier != content);
        if replaced {
            self.replaced.insert(ArchivePath(path.as_str().to_owned()));
        }
        self.files.insert(path, content);
        Ok(())
    }

    /// Decodes one regular file through a fixed buffer, checking its declared size, and
    /// records the omission in place of an earlier file of its path.
    fn omit(
        &mut self,
        path: ArchivePath,
        reason: ArchiveOmissionReason,
        size: u64,
        reader: &mut impl Read,
    ) -> Result<(), ArchiveError> {
        let mut writer = DigestWriter::default();
        let read = std::io::copy(&mut reader.take(size + 1), &mut writer)
            .map_err(|_| ArchiveError::InvalidArchive)?;
        if read != size {
            return Err(ArchiveError::InvalidArchive);
        }
        let content = OmittedContent {
            kind: ArchiveMemberKind::File,
            reason,
            size,
            digest: writer.0.finalize().into(),
        };
        let replaced = self.files.remove(path.as_str()).is_some()
            || self
                .omitted
                .get(&path)
                .is_some_and(|earlier| *earlier != content);
        if replaced {
            self.replaced.insert(path.clone());
        }
        self.omitted.insert(path, content);
        Ok(())
    }

    /// Registers a symbolic or hard link entry's path without reading its target or content.
    /// The link never becomes a file and its target is never opened.
    fn push_skipped_link(&mut self, path: &str) -> Result<(), ArchiveError> {
        let kind = ArchiveMemberKind::Link;
        match self.accept_path(path, kind)? {
            RelativePath::NonCanonicalUnicode(path) => self.omit_empty(path, kind),
            RelativePath::Canonical(relative) => {
                let path = RelativePath::project_path(relative)
                    .ok_or_else(|| ArchiveError::unsafe_path(path))?;
                self.skipped_links.push(path);
            }
        }
        Ok(())
    }

    /// Records a directory or link whose name does not use Unicode NFC.
    fn omit_empty(&mut self, path: ArchivePath, kind: ArchiveMemberKind) {
        let content = OmittedContent {
            kind,
            reason: ArchiveOmissionReason::NonCanonicalUnicode,
            size: 0,
            digest: Sha256::digest([]).into(),
        };
        self.omitted.insert(path, content);
    }

    fn into_files(mut self, digest: FileDigest) -> ArchiveFiles {
        self.seen
            .retain(|path, _| ProjectPath::new(path.as_str()).is_ok());
        ArchiveFiles {
            digest,
            files: self.files,
            members: self.seen,
            skipped_links: self.skipped_links,
            replaced: self.replaced,
            omissions: self
                .omitted
                .into_iter()
                .map(|(path, content)| ArchiveOmission {
                    path,
                    kind: content.kind,
                    reason: content.reason,
                    size: content.size,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
#[path = "archive_tests.rs"]
mod tests;
