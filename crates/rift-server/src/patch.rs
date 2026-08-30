//! Unified-diff patch engine: parses hunks with `diffy`, then locates and
//! applies each one itself, following `git apply` semantics.
//!
//! A hunk header is a hint, not a requirement, on both of its halves.
//! Context and deleted lines must match by content, but the position they
//! match at may drift from the header: each hunk's search starts at its
//! header position corrected by the drift already discovered from earlier
//! hunks in the same file - the running delta `git apply` itself carries
//! forward - then widens outward, nearer positions first and the earlier
//! position on a tie. The header's line counts are derived from the hunk's
//! own body before parsing, the way `git apply` derives them, so a
//! miscounted header applies and only a body that cannot be located
//! refuses.
//!
//! Matching compares line content alone, the way `git apply` tolerates it: an LF hunk
//! locates against CRLF source, and the reverse. A located hunk's context lines keep the
//! exact bytes already standing in the source, and its inserted lines take the line
//! ending already prevailing at that position, so bytes outside the hunk stay unchanged
//! and no hunk introduces a foreign ending. A hunk whose content cannot be located
//! anywhere in the file refuses, naming each side's content and line ending, so two
//! genuinely different byte strings never render identically.
//!
//! Each located hunk records the previous image's bytes it replaced, so a
//! result names the regions that changed instead of echoing whole files.
//! `/dev/null` headers create or delete a file; renames and copies stay
//! unsupported.
//!
//! An existing target is any file the workspace's `[source]` policy makes visible,
//! parsed or not: the syntax index's own copy, then the text index's, then a direct
//! filesystem read guarded by the policy alone. The base image is always the file's
//! bytes on disk; hunk context is the guard and needs no syntax tree. A path that
//! resolves to a directory refuses `target_is_file` instead of reaching a read or a
//! write that would otherwise fail unclassified.

use std::fs;
use std::path::Path;

use diffy::{Hunk, HunkRange, Line, ParsePatchError, Patch};
use rift_core::ProjectPath as CoreProjectPath;
use rift_core::line;
use rift_index::TextSourceFile;
use rift_protocol::change::{
    ChangeResult, OperationPrecondition, OperationPreconditionKind, OperationPreconditionStatus,
    PreconditionValue, RefusalReason,
};

use crate::read::{ReadError, ReadFault, ReadService, digest_hex8};
use crate::rewrite::FileRewrite;

/// Most files one unified diff may address.
pub(crate) const PATCH_FILES_MAX: usize = 64;

/// Longest hunk-mismatch detail one precondition value carries.
const PATCH_MISMATCH_DETAIL_BYTES_MAX: usize = 256;

/// Opens a unified diff's original-file header line, such as `--- a/src/lib.rs`.
const ORIGINAL_HEADER_PREFIX: &str = "--- ";

/// Opens a unified diff's modified-file header line, such as `+++ b/src/lib.rs`.
const MODIFIED_HEADER_PREFIX: &str = "+++ ";

/// Opens a unified diff's hunk header line, such as `@@ -1,2 +1,3 @@`.
const HUNK_HEADER_PREFIX: &str = "@@";

/// The header path marking that a segment creates or deletes its file
/// instead of editing an existing one.
const NULL_TARGET: &str = "/dev/null";

/// Opens the `apply_patch` envelope some agents emit; not a unified diff.
const APPLY_PATCH_ENVELOPE_OPENING: &str = "*** Begin Patch";

/// The unified-diff form every shape refusal names.
const UNIFIED_DIFF_EXPECTED_FORM: &str = "send a unified diff that opens each file with `--- a/src/lib.rs` and `+++ b/src/lib.rs` headers followed by `@@ -1,3 +1,4 @@` hunks, where context lines start with a space, removed lines with `-`, and added lines with `+`";

/// Why a patch body is not a unified diff `patch` reads.
#[derive(Debug, PartialEq, Eq)]
enum PatchShapeViolation {
    /// The body opens with the `apply_patch` envelope instead of a file header.
    ApplyPatchEnvelope,
    /// The body carries no file header at all.
    NoFileHeaders,
    /// The body carries a file header no hunk follows, and no `NULL_TARGET`
    /// header, which creates or deletes its file without one.
    NoHunks,
    /// The body addresses more files than one diff may carry.
    TooManyFiles { file_count: usize },
}

impl std::fmt::Display for PatchShapeViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApplyPatchEnvelope => write!(
                formatter,
                "is an `{APPLY_PATCH_ENVELOPE_OPENING}` envelope, which `patch` does not read: {UNIFIED_DIFF_EXPECTED_FORM}"
            ),
            Self::NoFileHeaders => {
                write!(
                    formatter,
                    "carries no file headers: {UNIFIED_DIFF_EXPECTED_FORM}"
                )
            }
            Self::NoHunks => write!(
                formatter,
                "carries a `{ORIGINAL_HEADER_PREFIX}a/path` file header with no `{HUNK_HEADER_PREFIX}` hunk after it: {UNIFIED_DIFF_EXPECTED_FORM}"
            ),
            Self::TooManyFiles { file_count } => write!(
                formatter,
                "addresses {file_count} files, more than {PATCH_FILES_MAX} files one diff may carry: {UNIFIED_DIFF_EXPECTED_FORM}"
            ),
        }
    }
}

/// Whether `text` is a file header naming [`NULL_TARGET`] in place of a path.
fn names_null_target(text: &str) -> bool {
    text.strip_prefix(ORIGINAL_HEADER_PREFIX)
        .or_else(|| text.strip_prefix(MODIFIED_HEADER_PREFIX))
        == Some(NULL_TARGET)
}

/// Classifies why `patch` is not a unified diff `patch` reads, in precedence
/// order: an `apply_patch` envelope, then a body carrying no file header, then
/// more files than the bound, then a file header no hunk follows. A creation or
/// deletion carries no hunk of its own, so a [`NULL_TARGET`] header excuses the
/// last arm.
fn patch_shape_violation(patch: &str) -> Option<PatchShapeViolation> {
    let opening = line::lines_inclusive(patch)
        .map(line::without_ending)
        .find(|text| !text.trim().is_empty())
        .unwrap_or_default();
    let (_, segments) = split_at_marker(patch, |text| text.starts_with(ORIGINAL_HEADER_PREFIX));
    let carries_hunk =
        line::lines_inclusive(patch).any(|text| text.starts_with(HUNK_HEADER_PREFIX));
    let carries_null_target = line::lines_inclusive(patch)
        .map(line::without_ending)
        .any(names_null_target);
    match segments.len() {
        _ if opening.starts_with(APPLY_PATCH_ENVELOPE_OPENING) => {
            Some(PatchShapeViolation::ApplyPatchEnvelope)
        }
        0 => Some(PatchShapeViolation::NoFileHeaders),
        file_count if file_count > PATCH_FILES_MAX => {
            Some(PatchShapeViolation::TooManyFiles { file_count })
        }
        _ if !carries_hunk && !carries_null_target => Some(PatchShapeViolation::NoHunks),
        _ => None,
    }
}

/// Splits one unified diff into its per-file segments. Only a header line
/// opens a segment: hunk body lines never start with `---` at column zero,
/// because context lines carry a leading space and removals a single `-`.
///
/// Hunk body bytes pass through untouched - a CRLF patch keeps its `\r`
/// so its context matches a CRLF source, and an ending mismatch surfaces
/// as hunk-context drift, never a silent rewrite. Only structural lines
/// change: they shed a CRLF ending, because the diff parser rejects `\r`
/// in headers, and a hunk header's counts are replaced by the counts its
/// own body carries.
///
/// # Errors
///
/// Returns [`ReadError`] naming the [`PatchShapeViolation`] when `patch` is
/// not a unified diff this function can split.
pub(crate) fn split_file_segments(patch: &str) -> Result<Vec<String>, ReadError> {
    if let Some(violation) = patch_shape_violation(patch) {
        return Err(ReadFault::invalid("patch", violation.to_string()));
    }
    let (_, raw_segments) = split_at_marker(patch, |text| text.starts_with(ORIGINAL_HEADER_PREFIX));
    Ok(raw_segments
        .iter()
        .map(|raw| recounted_headers(&normalize_segment(raw)))
        .collect())
}

/// Rewrites every hunk header's line counts to the counts the hunk's own
/// body carries, leaving its offsets alone.
///
/// `git apply` derives the counts this way, and an agent composing a diff
/// by hand counts context, removals, and the blank line between two
/// declarations. A header whose counts disagree with its body used to
/// refuse the whole patch before a single line was compared; now only a
/// body that cannot be located in the file refuses.
fn recounted_headers(segment: &str) -> String {
    let (prefix, hunks) = split_into_hunks(segment);
    let mut recounted = prefix;
    for hunk in &hunks {
        push_recounted_hunk(&mut recounted, hunk);
    }
    recounted
}

/// Appends one hunk to `recounted`, its header carrying the counts its own
/// body implies. A header that is not the `@@ -old +new @@` shape passes
/// through untouched, leaving the parser to name what is wrong with it.
fn push_recounted_hunk(recounted: &mut String, hunk: &str) {
    let mut lines = line::lines_inclusive(hunk);
    // A chunk always opens with its own `@@` line; an empty chunk carries no
    // header to rewrite and no body to count, and falls through as itself.
    let header = lines.next().unwrap_or_default();
    let body: Vec<&str> = lines.collect();
    match rewritten_header(header, HunkCounts::of(&body)) {
        Some(rewritten) => recounted.push_str(&rewritten),
        None => recounted.push_str(header),
    }
    for text in body {
        recounted.push_str(text);
    }
}

/// The line counts one hunk's body implies for each side of its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HunkCounts {
    old: usize,
    new: usize,
}

impl HunkCounts {
    /// Counts `body` the way `git apply` does: a line opening with a space
    /// stands on both sides, `-` on the old side alone, `+` on the new side
    /// alone, and a bare empty line is context both sides carry. The
    /// `\ No newline at end of file` marker describes the line above it and
    /// stands on neither side.
    fn of(body: &[&str]) -> Self {
        let mut counts = Self { old: 0, new: 0 };
        for text in body {
            match line::without_ending(text).as_bytes().first() {
                Some(b' ') | None => {
                    counts.old += 1;
                    counts.new += 1;
                }
                Some(b'-') => counts.old += 1,
                Some(b'+') => counts.new += 1,
                _ => {}
            }
        }
        counts
    }
}

/// Rewrites `header`'s counts to `counts`, keeping its offsets, its
/// trailing section heading, and its own line ending. Returns `None` for
/// anything that is not the `@@ -old +new @@` shape.
fn rewritten_header(header: &str, counts: HunkCounts) -> Option<String> {
    let text = line::without_ending(header);
    let ending = &header[text.len()..];
    let (ranges, trailing) = text
        .strip_prefix(HUNK_HEADER_PREFIX)?
        .split_once(HUNK_HEADER_PREFIX)?;
    let mut sides = ranges.split_whitespace();
    let old_start = declared_start(sides.next()?, '-')?;
    let new_start = declared_start(sides.next()?, '+')?;
    if sides.next().is_some() {
        return None;
    }
    Some(format!(
        "{HUNK_HEADER_PREFIX} -{old_start},{} +{new_start},{} {HUNK_HEADER_PREFIX}{trailing}{ending}",
        counts.old, counts.new
    ))
}

/// The start offset one `-old` or `+new` header side declares, dropping
/// the count the body now decides.
fn declared_start(side: &str, sign: char) -> Option<u64> {
    side.strip_prefix(sign)?
        .split(',')
        .next()?
        .parse::<u64>()
        .ok()
}

/// Sheds each of `raw`'s structural line endings down to a bare `\n`,
/// leaving body-line bytes untouched. See [`push_segment_line`] for which
/// lines count as structural and why.
fn normalize_segment(raw: &str) -> String {
    let mut segment = String::new();
    for (index, text) in line::lines_inclusive(raw).enumerate() {
        let structural = index == 0
            || (index == 1 && text.starts_with(MODIFIED_HEADER_PREFIX))
            || text.starts_with(HUNK_HEADER_PREFIX);
        push_segment_line(&mut segment, text, structural);
    }
    segment
}

/// Appends one diff line to its segment. A structural line - the `--- ` and
/// `+++ ` file headers and `@@` hunk headers - sheds a CRLF ending down to
/// bare `\n`, because the diff parser rejects `\r` there; body lines keep
/// their exact bytes, CRLF included, so hunk content matches the stored
/// source byte-for-byte.
fn push_segment_line(segment: &mut String, line: &str, structural: bool) {
    match (structural, line.strip_suffix(rift_core::line::CRLF)) {
        (true, Some(stripped)) => {
            segment.push_str(stripped);
            segment.push_str(rift_core::line::LineEnding::Lf.as_str());
        }
        _ => segment.push_str(line),
    }
}

/// Scans `text` line by line through [`line::lines_inclusive`], opening a
/// new segment whenever `opens_segment` matches a line, and returns the
/// bytes before the first match alongside each segment's own bytes.
fn split_at_marker(text: &str, opens_segment: impl Fn(&str) -> bool) -> (String, Vec<String>) {
    let mut prefix = String::new();
    let mut segments: Vec<String> = Vec::new();
    for text_line in line::lines_inclusive(text) {
        if opens_segment(text_line) {
            segments.push(String::new());
        }
        match segments.last_mut() {
            Some(segment) => segment.push_str(text_line),
            None => prefix.push_str(text_line),
        }
    }
    (prefix, segments)
}

/// Resolves one file segment to its rewrite plan, or the refusal for a
/// context that cannot be located, a rename or copy, or a target this
/// segment's mode does not allow.
///
/// # Errors
///
/// Returns [`ReadError`] when the segment does not parse, addresses an
/// illegal path, or the filesystem cannot be read.
pub(crate) fn resolve_segment(
    root: &Path,
    reads: &ReadService,
    segment: &str,
    ordinal: usize,
) -> Result<Result<FileRewrite, ChangeResult>, ReadError> {
    let parsed = parse_segment(segment, ordinal)?;
    match resolve_patch_target(&parsed)? {
        Err(refusal) => Ok(Err(refusal)),
        Ok(PatchTarget::Modify(path)) => resolve_modify(root, reads, &path, &parsed),
        Ok(PatchTarget::Create(path)) => resolve_create(root, reads, &path, &parsed),
        Ok(PatchTarget::Delete(path)) => resolve_delete(root, reads, &path, &parsed),
    }
}

/// Parses one file segment, naming the specific hunk a line-count mismatch
/// broke when the segment carries more than one.
fn parse_segment(segment: &str, ordinal: usize) -> Result<Patch<'_, str>, ReadError> {
    Patch::from_str(segment).map_err(|error| named_parse_error(segment, ordinal, &error))
}

/// Re-parses each hunk in isolation to find the one a whole-segment parse
/// error came from. A hunk whose own line counts are self-inconsistent
/// fails alone exactly as it failed within the segment; an error that only
/// exists between hunks (such as out-of-order ranges) fails no isolated
/// hunk, so the fallback names the file instead.
fn named_parse_error(segment: &str, ordinal: usize, error: &ParsePatchError) -> ReadError {
    let (header, hunks) = split_into_hunks(segment);
    for (hunk_index, hunk) in hunks.iter().enumerate() {
        let candidate = format!("{header}{hunk}");
        if Patch::from_str(&candidate).is_err() {
            return ReadFault::invalid(
                "patch",
                format!("file {ordinal} hunk {}: {error}", hunk_index + 1),
            );
        }
    }
    ReadFault::invalid("patch", format!("file {ordinal}: {error}"))
}

/// Splits one file segment into its shared header and each hunk's own
/// text, so a hunk can be re-parsed against the header alone.
fn split_into_hunks(segment: &str) -> (String, Vec<String>) {
    split_at_marker(segment, |text| text.starts_with(HUNK_HEADER_PREFIX))
}

/// What one parsed file segment addresses.
enum PatchTarget {
    /// An existing file's content changes.
    Modify(CoreProjectPath),
    /// A `/dev/null` original header: the segment creates this path.
    Create(CoreProjectPath),
    /// A `/dev/null` modified header: the segment deletes this path.
    Delete(CoreProjectPath),
}

/// Resolves the path and mode one parsed segment addresses, or the
/// refusal for a rename or copy, which this release does not serve.
///
/// Patch paths are wire values, not OS paths: forward-slash relative on
/// every platform, with git's literal `a/`, `b/`, and `/dev/null`
/// conventions, which git emits unchanged on Windows and macOS alike.
fn resolve_patch_target(
    parsed: &Patch<'_, str>,
) -> Result<Result<PatchTarget, ChangeResult>, ReadError> {
    let original = parsed.original().unwrap_or_default();
    let modified = parsed.modified().unwrap_or_default();
    if original.contains('\\') || modified.contains('\\') {
        return Err(ReadFault::invalid(
            "patch",
            "path uses backslash separators; project paths are forward-slash \
             relative on every platform, such as `src/lib.rs`",
        ));
    }
    let target = match (original == NULL_TARGET, modified == NULL_TARGET) {
        (true, true) => {
            return Ok(Err(ChangeResult::refused(
                RefusalReason::Unsupported,
                Vec::new(),
            )));
        }
        (true, false) => PatchTarget::Create(project_path(strip_prefix(modified, "b/"))?),
        (false, true) => PatchTarget::Delete(project_path(strip_prefix(original, "a/"))?),
        (false, false) => {
            let original = strip_prefix(original, "a/");
            let modified = strip_prefix(modified, "b/");
            if original != modified {
                return Ok(Err(ChangeResult::refused(
                    RefusalReason::Unsupported,
                    Vec::new(),
                )));
            }
            PatchTarget::Modify(project_path(original)?)
        }
    };
    Ok(Ok(target))
}

fn strip_prefix<'a>(value: &'a str, prefix: &str) -> &'a str {
    value.strip_prefix(prefix).unwrap_or(value)
}

fn project_path(value: &str) -> Result<CoreProjectPath, ReadError> {
    CoreProjectPath::new(value).map_err(|error| {
        ReadFault::invalid("patch", rift_core::fault_label(&error.fault().violation()))
    })
}

/// Resolves a segment that edits an existing file: the syntax or text index's own
/// copy when the path is indexed, the `[source]` policy against the filesystem
/// otherwise.
fn resolve_modify(
    root: &Path,
    reads: &ReadService,
    path: &CoreProjectPath,
    parsed: &Patch<'_, str>,
) -> Result<Result<FileRewrite, ChangeResult>, ReadError> {
    let base = match resolve_base_image(root, reads, path)? {
        Ok(base) => base,
        Err(refusal) => return Ok(Err(refusal)),
    };
    match apply_segment(&base, parsed) {
        Ok(applied) => Ok(Ok(FileRewrite::modify(
            path.clone(),
            &base,
            applied.next_source,
        ))),
        Err(detail) => Ok(Err(detail.into_refusal(path))),
    }
}

/// The bytes the index already holds for `path`: the syntax index's copy, then the
/// text index's copy. `None` when neither indexes it.
fn indexed_source<'a>(reads: &'a ReadService, path: &CoreProjectPath) -> Option<&'a str> {
    if let Some(file) = reads.index().file(path) {
        return Some(file.source());
    }
    reads.index().text_file(path).map(TextSourceFile::content)
}

/// Compares `indexed`'s stored copy against the disk read at `absolute`, taking the
/// disk bytes as the base image on agreement and refusing `source_unchanged` on
/// disagreement.
fn drift_checked_base(
    path: &CoreProjectPath,
    indexed: &str,
    absolute: &Path,
) -> Result<Result<String, ChangeResult>, ReadError> {
    let disk = fs::read_to_string(absolute)
        .map_err(|error| ReadFault::storage(path.as_str(), "read", &error))?;
    if disk == indexed {
        Ok(Ok(disk))
    } else {
        Ok(Err(source_drift_refusal(path, indexed, &disk)))
    }
}

/// Reads `path` fresh from disk through the `[source]` policy alone when current index
/// does not hold it. A path the policy excludes refuses `unsupported`,
/// naming the policy; a visible path absent from the filesystem refuses
/// `target_exists` the way an unindexed path already does; a directory refuses
/// `target_is_file` instead of reaching the read that would otherwise fail unclassified.
fn visible_source(
    reads: &ReadService,
    path: &CoreProjectPath,
    absolute: &Path,
) -> Result<Result<String, ChangeResult>, ReadError> {
    let visible = reads
        .source_policy()
        .is_some_and(|policy| policy.visible(absolute));
    if !visible {
        return Ok(Err(crate::publish::not_visible_refusal(path)));
    }
    let metadata = match fs::metadata(absolute) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Err(target_missing_refusal(path)));
        }
        Err(error) => return Err(ReadFault::storage(path.as_str(), "stat", &error)),
    };
    if metadata.is_dir() {
        return Ok(Err(directory_target_refusal(path)));
    }
    match fs::read_to_string(absolute) {
        Ok(disk) => Ok(Ok(disk)),
        Err(error) => Err(ReadFault::storage(path.as_str(), "read", &error)),
    }
}

/// Resolves `path`'s current base image for a patch to apply against: the syntax
/// index's copy, then the text index's copy, each checked for drift against the
/// disk; the `[source]` policy against the filesystem last, for a visible file no
/// provider parses. The base image is always the file's bytes on disk - hunk context
/// is the guard and needs no syntax tree. `resolve_modify` and `resolve_delete` share
/// this resolution; only what they do with the result differs.
fn resolve_base_image(
    root: &Path,
    reads: &ReadService,
    path: &CoreProjectPath,
) -> Result<Result<String, ChangeResult>, ReadError> {
    let absolute = root.join(path.as_str());
    match indexed_source(reads, path) {
        Some(indexed) => drift_checked_base(path, indexed, &absolute),
        None => visible_source(reads, path, &absolute),
    }
}

/// Resolves a segment that creates a new file. The starting image is
/// empty, so a hunk carrying context or deleted lines cannot locate a
/// position and refuses exactly as a genuine mismatch would.
///
/// Resolves the target through the write gate first: a create into a
/// path the `[source]` policy excludes refuses the same way a modify
/// already does, rather than reaching the filesystem at all.
fn resolve_create(
    root: &Path,
    reads: &ReadService,
    path: &CoreProjectPath,
    parsed: &Patch<'_, str>,
) -> Result<Result<FileRewrite, ChangeResult>, ReadError> {
    if let Err(refusal) = crate::publish::resolve_write_target(
        reads,
        root,
        path,
        crate::publish::SymlinkResolution::Resolve,
    )? {
        return Ok(Err(refusal));
    }
    if let Some(refusal) = creation_conflict(root, path)? {
        return Ok(Err(refusal));
    }
    match apply_segment("", parsed) {
        Ok(applied) => Ok(Ok(FileRewrite::create(path.clone(), applied.next_source))),
        Err(detail) => Ok(Err(detail.into_refusal(path))),
    }
}

/// The refusal for a create target something already occupies: `target_is_file` when a
/// directory stands there, `target_exists` for anything else - a file or a symlink.
/// `None` when nothing occupies the path.
fn creation_conflict(
    root: &Path,
    path: &CoreProjectPath,
) -> Result<Option<ChangeResult>, ReadError> {
    match fs::metadata(root.join(path.as_str())) {
        Ok(metadata) if metadata.is_dir() => Ok(Some(directory_target_refusal(path))),
        Ok(_) => Ok(Some(already_exists_refusal(path))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ReadFault::storage(path.as_str(), "stat", &error)),
    }
}

/// Resolves a segment that deletes an existing file: the hunks must consume the
/// entire file, leaving nothing behind. Shares [`resolve_base_image`] with
/// `resolve_modify`, so an unparsed visible file deletes the same way it modifies.
fn resolve_delete(
    root: &Path,
    reads: &ReadService,
    path: &CoreProjectPath,
    parsed: &Patch<'_, str>,
) -> Result<Result<FileRewrite, ChangeResult>, ReadError> {
    let base = match resolve_base_image(root, reads, path)? {
        Ok(base) => base,
        Err(refusal) => return Ok(Err(refusal)),
    };
    match apply_segment(&base, parsed) {
        Ok(applied) if applied.next_source.is_empty() => {
            Ok(Ok(FileRewrite::delete(path.clone(), &base)))
        }
        Ok(applied) => Ok(Err(deletion_incomplete_refusal(path, &applied.next_source))),
        Err(detail) => Ok(Err(detail.into_refusal(path))),
    }
}

/// One line of the file image a patch's hunks apply against: bytes still
/// from the original file, or bytes a previous hunk in this same run
/// already wrote - context lines included, so a later hunk never matches
/// into a `Patched` line even when its content is unchanged; that would
/// apply on top of text this same patch just introduced.
enum ImageLine<'a> {
    Original(&'a str),
    Patched(String),
}

impl ImageLine<'_> {
    fn text(&self) -> &str {
        match self {
            Self::Original(text) => text,
            Self::Patched(text) => text,
        }
    }

    fn is_patched(&self) -> bool {
        matches!(self, Self::Patched(_))
    }
}

/// One segment applied: the file's whole next content.
pub(crate) struct AppliedSegment {
    pub(crate) next_source: String,
}

/// Applies every hunk in `parsed` against `starting`, in order.
///
/// Each hunk's search anchors at its header position, corrected by the
/// drift already found while applying earlier hunks in this same segment.
/// The search itself never fuzzy-matches content: only line-content
/// equality locates a hunk, ignoring each side's line ending, at a
/// position that may differ from the header. Bounded: one hunk visits at
/// most `image.len() + 1` candidate positions, since the search distance
/// never usefully exceeds the image's length.
fn apply_segment(
    starting: &str,
    parsed: &Patch<'_, str>,
) -> Result<AppliedSegment, MismatchDetail> {
    let mut image: Vec<ImageLine<'_>> = base_image(starting);
    let mut delta: i64 = 0;
    for (index, hunk) in parsed.hunks().iter().enumerate() {
        let pre_image = pre_image_lines(hunk);
        let expected = zero_based_start(hunk.new_range());
        let anchor = clamp_anchor(expected, delta, image.len());
        let Some(found) = find_hunk_position(&image, &pre_image, anchor) else {
            return Err(MismatchDetail::new(
                index + 1,
                hunk,
                anchor,
                &pre_image,
                &image,
            ));
        };
        delta = i64::try_from(found).unwrap_or(0) - i64::try_from(expected).unwrap_or(0);
        let post_lines = reconciled_post_lines(hunk, &image, found);
        image.splice(found..found + pre_image.len(), post_lines);
    }
    Ok(AppliedSegment {
        next_source: image.iter().map(ImageLine::text).collect(),
    })
}

/// The lines a hunk leaves behind at its located position: each context line keeps the
/// exact source bytes it matched there, each inserted line takes the hunk's prevailing
/// line ending - the ending the file already carries at that position, so content located
/// by matching alone never introduces a foreign line ending - and each deleted line
/// contributes nothing.
fn reconciled_post_lines<'a>(
    hunk: &Hunk<'_, str>,
    image: &[ImageLine<'a>],
    found: usize,
) -> Vec<ImageLine<'a>> {
    let ending = prevailing_ending(image, found);
    let mut cursor = found;
    let mut lines = Vec::with_capacity(hunk.lines().len());
    for line in hunk.lines().iter().copied() {
        match line {
            Line::Context(_) => {
                if let Some(source) = image.get(cursor) {
                    lines.push(ImageLine::Patched(source.text().to_owned()));
                }
                cursor += 1;
            }
            Line::Delete(_) => cursor += 1,
            Line::Insert(text) => {
                lines.push(ImageLine::Patched(ending_reconciled(text, ending)));
            }
        }
    }
    lines
}

/// The line ending inserted content should adopt for a hunk located at `found`: the
/// ending the source line standing at that position already carries, or the ending of the
/// line immediately before it when the hunk lands at the file's end. `None` when neither
/// exists - an empty file with nothing to match - so inserted content keeps whatever
/// ending the diff itself carries.
fn prevailing_ending(image: &[ImageLine<'_>], found: usize) -> Option<line::LineEnding> {
    image
        .get(found)
        .or_else(|| found.checked_sub(1).and_then(|prior| image.get(prior)))
        .and_then(|entry| line::LineEnding::of(entry.text()))
}

/// `text`'s content with its ending replaced by `ending`, when `text` carries an ending
/// and it differs from `ending`; `text` unchanged otherwise, including when it carries no
/// ending at all - a final line with no trailing newline stays that way.
fn ending_reconciled(text: &str, ending: Option<line::LineEnding>) -> String {
    match (ending, line::LineEnding::of(text)) {
        (Some(ending), Some(current)) if current != ending => {
            format!("{}{}", line::without_ending(text), ending.as_str())
        }
        _ => text.to_owned(),
    }
}

/// The starting image: every line of `starting`.
fn base_image(starting: &str) -> Vec<ImageLine<'_>> {
    line::lines_inclusive(starting)
        .map(ImageLine::Original)
        .collect()
}

/// Converts a hunk's declared old-file range into a 0-based image
/// position: the range's start is 1-based when it covers any lines, and
/// already 0-based (the insertion point) when it covers none.
fn zero_based_start(range: HunkRange) -> usize {
    if range.is_empty() {
        range.start()
    } else {
        range.start().saturating_sub(1)
    }
}

/// Biases a hunk's expected position by the drift found so far, clamped
/// to the current image so an out-of-range header cannot panic the
/// subtraction that follows.
fn clamp_anchor(expected: usize, delta: i64, image_len: usize) -> usize {
    let expected = i64::try_from(expected).unwrap_or(i64::MAX);
    let image_len = i64::try_from(image_len).unwrap_or(i64::MAX);
    let biased = expected.saturating_add(delta).clamp(0, image_len);
    usize::try_from(biased).unwrap_or(0)
}

/// Searches outward from `anchor` for the unique run of context and
/// deleted lines this hunk expects: the anchor itself first, then each
/// increasing distance with the backward position tried before the
/// forward one, so an equal-distance tie favors the earlier position.
///
/// Bounded: at most `image.len() + 1` positions are compared, since no
/// valid position lies further than the image's own length from anchor.
fn find_hunk_position(image: &[ImageLine<'_>], pre_image: &[&str], anchor: usize) -> Option<usize> {
    let max_start = image.len().checked_sub(pre_image.len())?;
    let anchor = anchor.min(max_start);
    if matches_at(image, pre_image, anchor) {
        return Some(anchor);
    }
    for distance in 1..=image.len() {
        if let Some(back) = anchor.checked_sub(distance)
            && matches_at(image, pre_image, back)
        {
            return Some(back);
        }
        let forward = anchor + distance;
        if forward <= max_start && matches_at(image, pre_image, forward) {
            return Some(forward);
        }
    }
    None
}

/// Tests whether `pre_image` matches the image at `pos`: every line equal by content,
/// ignoring each side's line ending, and none of them already written by an earlier hunk.
fn matches_at(image: &[ImageLine<'_>], pre_image: &[&str], pos: usize) -> bool {
    let Some(window) = image.get(pos..pos + pre_image.len()) else {
        return false;
    };
    if window.iter().any(ImageLine::is_patched) {
        return false;
    }
    window
        .iter()
        .map(ImageLine::text)
        .map(line::without_ending)
        .eq(pre_image.iter().copied().map(line::without_ending))
}

/// The lines a hunk expects to find at its position, in body order: context and deleted
/// lines - what the search in [`find_hunk_position`] must locate. Insert-only lines
/// contribute nothing, since they name no position in the source at all.
fn pre_image_lines<'a>(hunk: &Hunk<'a, str>) -> Vec<&'a str> {
    hunk.lines()
        .iter()
        .filter_map(|line| match line {
            Line::Context(text) | Line::Delete(text) => Some(*text),
            Line::Insert(_) => None,
        })
        .collect()
}

/// Renders the standard `@@ -old +new @@` git hunk header text, through
/// diffy's own `Display` for `hunk`'s old and new ranges.
fn hunk_header_text(hunk: &Hunk<'_, str>) -> String {
    format!(
        "{HUNK_HEADER_PREFIX} -{} +{} {HUNK_HEADER_PREFIX}",
        hunk.old_range(),
        hunk.new_range()
    )
}

/// What a hunk expected at the position it could not be found: the hunk
/// that failed, the line it was tried at, and what stood there instead.
struct MismatchDetail {
    ordinal: usize,
    header: String,
    line: usize,
    expected: String,
    observed: String,
}

impl MismatchDetail {
    /// Builds the detail for hunk `ordinal`'s failed search: the header it
    /// carries, the position `anchor` it was tried at, what it expected to
    /// find there, and what stood there instead.
    fn new(
        ordinal: usize,
        hunk: &Hunk<'_, str>,
        anchor: usize,
        pre_image: &[&str],
        image: &[ImageLine<'_>],
    ) -> Self {
        let expected = pre_image.first().copied().unwrap_or_default();
        let observed = image.get(anchor).map(ImageLine::text);
        Self {
            ordinal,
            header: hunk_header_text(hunk),
            line: anchor + 1,
            expected: escaped_line(expected),
            observed: observed.map_or_else(|| "end of file".to_owned(), escaped_line),
        }
    }

    fn into_refusal(self, path: &CoreProjectPath) -> ChangeResult {
        precondition_refusal(
            OperationPreconditionKind::SourceUnchanged,
            path,
            PreconditionValue::Text {
                value: truncate_detail(self.side_text("expected", &self.expected)),
            },
            PreconditionValue::Text {
                value: truncate_detail(self.side_text("found", &self.observed)),
            },
        )
    }

    fn side_text(&self, label: &str, content: &str) -> String {
        format!(
            "hunk {} `{}`, line {}: {label} `{content}`",
            self.ordinal, self.header, self.line
        )
    }
}

/// Renders one line for a mismatch message: its content, stripped of its ending, followed
/// by that ending's own name. Two lines whose content agrees but whose bytes differ - a
/// `\r` present on one side alone - render as different strings, which stripping the
/// ending without naming it cannot do.
fn escaped_line(line: &str) -> String {
    let content = line::without_ending(line);
    let ending = match line::LineEnding::of(line) {
        Some(line::LineEnding::Lf) => "LF ending",
        Some(line::LineEnding::CrLf) => "CRLF ending",
        None => "no line ending",
    };
    format!("{content} ({ending})")
}

/// Truncates one detail string to the shared byte bound, never splitting
/// a UTF-8 character.
fn truncate_detail(mut value: String) -> String {
    let mut boundary = value.len().min(PATCH_MISMATCH_DETAIL_BYTES_MAX);
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

/// Builds an `UnmetPrecondition` refusal for `path`, naming `kind`'s
/// expected and observed values. Every fixed-shape single-precondition
/// refusal in this module routes through this constructor, so their
/// differences stay only in `kind`, `expected`, and `observed`.
fn precondition_refusal(
    kind: OperationPreconditionKind,
    path: &CoreProjectPath,
    expected: PreconditionValue,
    observed: PreconditionValue,
) -> ChangeResult {
    ChangeResult::refused(
        RefusalReason::UnmetPrecondition,
        vec![OperationPrecondition::new(
            kind,
            OperationPreconditionStatus::Failed,
            Vec::new(),
            vec![path.as_str().to_owned()],
            expected,
            observed,
        )],
    )
}

fn target_missing_refusal(path: &CoreProjectPath) -> ChangeResult {
    precondition_refusal(
        OperationPreconditionKind::TargetExists,
        path,
        PreconditionValue::Boolean { value: true },
        PreconditionValue::Boolean { value: false },
    )
}

fn already_exists_refusal(path: &CoreProjectPath) -> ChangeResult {
    precondition_refusal(
        OperationPreconditionKind::TargetExists,
        path,
        PreconditionValue::Boolean { value: false },
        PreconditionValue::Boolean { value: true },
    )
}

fn directory_target_refusal(path: &CoreProjectPath) -> ChangeResult {
    precondition_refusal(
        OperationPreconditionKind::TargetIsFile,
        path,
        PreconditionValue::Boolean { value: true },
        PreconditionValue::Boolean { value: false },
    )
}

fn source_drift_refusal(path: &CoreProjectPath, indexed: &str, disk: &str) -> ChangeResult {
    precondition_refusal(
        OperationPreconditionKind::SourceUnchanged,
        path,
        PreconditionValue::Text {
            value: digest_hex8(indexed),
        },
        PreconditionValue::Text {
            value: digest_hex8(disk),
        },
    )
}

fn deletion_incomplete_refusal(path: &CoreProjectPath, remaining: &str) -> ChangeResult {
    let lines = line::lines_inclusive(remaining).count();
    precondition_refusal(
        OperationPreconditionKind::SourceUnchanged,
        path,
        PreconditionValue::Text {
            value: "file fully deleted".to_owned(),
        },
        PreconditionValue::Text {
            value: truncate_detail(format!("{lines} line(s) remain after applying the hunks")),
        },
    )
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use diffy::Patch;
    use rift_core::SourceVisibility;
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::change::{
        BodySource, ChangeResult, ChangeSummary, OperationPreconditionKind, PATCH_BYTES_MAX,
        PatchParams, PreconditionValue, RefusalReason,
    };
    use rift_protocol::configuration::HistoryConfiguration;
    use rift_protocol::read::ProjectPath;

    use super::{
        CoreProjectPath, HunkCounts, PATCH_MISMATCH_DETAIL_BYTES_MAX, PatchTarget, apply_segment,
        creation_conflict, find_hunk_position, resolve_patch_target, rewritten_header,
        truncate_detail,
    };
    use crate::change::ChangeService;
    use crate::read::ReadService;
    use crate::rewrite::{FileRewrite, REWRITE_FILE_BYTES_MAX, RewriteKind};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn hunks(diff: &str) -> TestResult<Vec<String>> {
        Ok(super::split_file_segments(diff)?)
    }

    /// Builds a workspace with one `lib.rs`, and services over it.
    fn fixture(source: &str) -> TestResult<(tempfile::TempDir, ReadService, ChangeService)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), source)?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        Ok((directory, reads, changes))
    }

    fn applied_summary(result: ChangeResult) -> ChangeSummary {
        match result {
            ChangeResult::Applied { summary } => summary,
            ChangeResult::Refused { reason, .. } => {
                panic!("change must land, got refusal {reason:?}")
            }
            ChangeResult::Unchanged => panic!("change must land, got unchanged result"),
        }
    }

    #[test]
    fn apply_segment_matches_the_header_position_exactly() -> TestResult {
        let diff = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-one\n+ONE\n two\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let result = apply_segment("one\ntwo\n", &parsed)
            .map_err(|_| "must apply at the exact header position")?
            .next_source;
        assert_eq!(result, "ONE\ntwo\n");
        Ok(())
    }

    #[test]
    fn apply_segment_locates_a_hunk_drifted_forward_by_three_lines() -> TestResult {
        let diff = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-target\n+TARGET\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let source = "a\nb\nc\ntarget\n";
        let result = apply_segment(source, &parsed)
            .map_err(|_| "context three lines below the header must still be found")?
            .next_source;
        assert_eq!(result, "a\nb\nc\nTARGET\n");
        Ok(())
    }

    #[test]
    fn apply_segment_locates_a_hunk_drifted_backward_by_three_lines() -> TestResult {
        let diff = "--- a/f\n+++ b/f\n@@ -5 +5 @@\n-target\n+TARGET\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let source = "target\na\nb\n";
        let result = apply_segment(source, &parsed)
            .map_err(|_| "context three lines above the header must still be found")?
            .next_source;
        assert_eq!(result, "TARGET\na\nb\n");
        Ok(())
    }

    #[test]
    fn apply_segment_carries_the_running_delta_across_hunks() -> TestResult {
        // The diff was generated against a file starting at "one"; the real
        // file has an extra "zero" line ahead of everything the diff knows
        // about, so both hunks sit one line deeper than their headers say.
        let diff = "--- a/f\n+++ b/f\n\
             @@ -1,2 +1,3 @@\n one\n+inserted\n two\n\
             @@ -3 +4 @@\n-three\n+THREE\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let source = "zero\none\ntwo\nthree\n";
        let result = apply_segment(source, &parsed)
            .map_err(|_| "the second hunk must land after correcting for the first hunk's drift")?
            .next_source;
        assert_eq!(result, "zero\none\ninserted\ntwo\nTHREE\n");
        Ok(())
    }

    #[test]
    fn apply_segment_refuses_and_names_the_hunk_and_line_when_context_never_matches() -> TestResult
    {
        let diff = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-vanished\n+VANISHED\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let detail = apply_segment("one\ntwo\n", &parsed)
            .err()
            .ok_or("context that never appears in the file must refuse")?;
        assert_eq!(detail.ordinal, 1);
        assert_eq!(detail.line, 1);
        assert!(detail.header.contains("@@ -1 +1 @@"));
        assert_eq!(detail.expected, "vanished (LF ending)");
        assert_eq!(detail.observed, "one (LF ending)");
        Ok(())
    }

    /// Reproduces the defect a `without_ending`-stripped display left behind: content that
    /// matches at the reported line while a later line in the same hunk does not must still
    /// render `expected` and `observed` as different strings, never the identical `one`.
    #[test]
    fn mismatch_detail_names_each_sides_line_ending_when_the_first_line_matches_by_content_alone()
    -> TestResult {
        let diff = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n one\n-vanished\n+X\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let detail = apply_segment("one\r\ntwo\r\n", &parsed)
            .err()
            .ok_or("a hunk whose second line never matches must refuse")?;
        assert_eq!(detail.expected, "one (LF ending)");
        assert_eq!(detail.observed, "one (CRLF ending)");
        assert_ne!(
            detail.expected, detail.observed,
            "identical content with a differing line ending must never render as the same \
             string"
        );
        Ok(())
    }

    /// The file's last line carries no trailing newline at all; a mismatch reported there
    /// must still name that absence, not fall back to reporting some other ending.
    #[test]
    fn mismatch_detail_names_a_final_line_with_no_trailing_newline() -> TestResult {
        let diff = "--- a/f\n+++ b/f\n@@ -2 +2 @@\n-TWO\n+CHANGED\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let detail = apply_segment("one\ntwo", &parsed)
            .err()
            .ok_or("a final unterminated line that never matches must still refuse")?;
        assert_eq!(detail.expected, "TWO (LF ending)");
        assert_eq!(detail.observed, "two (no line ending)");
        Ok(())
    }

    #[test]
    fn find_hunk_position_breaks_an_equal_distance_tie_toward_the_earlier_position() {
        let image: Vec<super::ImageLine<'_>> = vec!["x", "match", "y", "match"]
            .into_iter()
            .map(super::ImageLine::Original)
            .collect();
        let pre_image = ["match"];
        let found = find_hunk_position(&image, &pre_image, 2);
        assert_eq!(
            found,
            Some(1),
            "positions 1 and 3 are equidistant from anchor 2; the earlier one wins"
        );
    }

    #[test]
    fn apply_segment_skips_a_position_a_prior_hunk_already_patched_even_on_a_tie() -> TestResult {
        // "a" appears at both line 1 and line 3; the first hunk patches line
        // 1 to "MARK". The second hunk's anchor sits equidistant from the
        // now-patched line 1 and the untouched line 3 - the backward
        // position is tried first per the tie-break rule, but it must be
        // rejected for being already patched, landing on line 3 instead.
        let diff = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+MARK\n@@ -2 +2 @@\n-a\n+ZULU\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let result = apply_segment("a\nz\na\n", &parsed)
            .map_err(|_| "the untouched occurrence must still be found")?
            .next_source;
        assert_eq!(result, "MARK\nz\nZULU\n");
        Ok(())
    }

    #[test]
    fn resolve_patch_target_creates_from_dev_null_original() -> TestResult {
        let diff = "--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1 @@\n+pub fn fresh() {}\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let target = resolve_patch_target(&parsed)?.map_err(|_| "creation must resolve")?;
        assert!(matches!(target, PatchTarget::Create(_)));
        Ok(())
    }

    #[test]
    fn resolve_patch_target_deletes_from_dev_null_modified() -> TestResult {
        let diff = "--- a/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-pub fn gone() {}\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let target = resolve_patch_target(&parsed)?.map_err(|_| "deletion must resolve")?;
        assert!(matches!(target, PatchTarget::Delete(_)));
        Ok(())
    }

    #[test]
    fn resolve_patch_target_refuses_a_rename_as_unsupported() -> TestResult {
        let diff = "--- a/old.rs\n+++ b/new.rs\n@@ -1 +1 @@\n-x\n+y\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let refusal = resolve_patch_target(&parsed)?
            .err()
            .ok_or("a rename must refuse")?;
        let ChangeResult::Refused { reason, .. } = refusal else {
            return Err("a rename refusal must carry the refused shape".into());
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        Ok(())
    }

    #[test]
    fn resolve_patch_target_refuses_when_both_sides_are_dev_null() -> TestResult {
        let diff = "--- /dev/null\n+++ /dev/null\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let refusal = resolve_patch_target(&parsed)?
            .err()
            .ok_or("a dev-null pair on both sides must refuse")?;
        let ChangeResult::Refused { reason, .. } = refusal else {
            return Err("a dev-null pair refusal must carry the refused shape".into());
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        Ok(())
    }

    #[test]
    fn resolve_patch_target_rejects_absolute_and_dotted_creation_paths() -> TestResult {
        for modified in ["b//etc/passwd", "b/../escape.rs"] {
            let diff = format!("--- /dev/null\n+++ {modified}\n@@ -0,0 +1 @@\n+x\n");
            let segments = hunks(&diff)?;
            let parsed = Patch::from_str(&segments[0])?;
            let error = resolve_patch_target(&parsed)
                .err()
                .ok_or_else(|| format!("path {modified} must be rejected"))?;
            assert_eq!(error.descriptor().code(), "invalid_request");
        }
        Ok(())
    }

    #[test]
    fn apply_segment_on_an_empty_starting_image_creates_the_file() -> TestResult {
        let diff = "--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,2 @@\n+one\n+two\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let content = apply_segment("", &parsed)
            .map_err(|_| "an all-addition hunk must apply")?
            .next_source;
        assert_eq!(content, "one\ntwo\n");
        Ok(())
    }

    #[test]
    fn apply_segment_refuses_creation_when_the_hunk_carries_context() -> TestResult {
        let diff = "--- /dev/null\n+++ b/new.rs\n@@ -1 +1,2 @@\n existing\n+two\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        apply_segment("", &parsed)
            .err()
            .ok_or("a context line cannot exist in an empty starting file")?;
        Ok(())
    }

    #[test]
    fn apply_segment_on_zero_hunks_leaves_the_starting_image_unchanged() -> TestResult {
        let diff = "--- /dev/null\n+++ b/empty.rs\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let content = apply_segment("", &parsed)
            .map_err(|_| "zero hunks must trivially apply")?
            .next_source;
        assert_eq!(content, "");
        Ok(())
    }

    #[test]
    fn named_parse_error_identifies_the_second_hunk_of_a_multi_hunk_segment() -> TestResult {
        let diff = "--- a/f\n+++ b/f\n\
             @@ -1 +1 @@\n-one\n+ONE\n\
             @@ -5,2 +5,2 @@\n-two\n+TWO\n";
        let error = super::parse_segment(diff, 3)
            .err()
            .ok_or("a self-inconsistent second hunk must fail to parse")?;
        assert!(
            error.to_string().contains("file 3 hunk 2"),
            "message must name the file and the specific hunk: {error}"
        );
        Ok(())
    }

    #[test]
    fn named_parse_error_falls_back_to_the_file_when_no_isolated_hunk_fails() -> TestResult {
        // Each hunk is internally consistent on its own; only their declared
        // ranges run backward, an error that exists between hunks and never
        // surfaces from re-parsing either hunk alone.
        let diff = "--- a/f\n+++ b/f\n\
             @@ -5 +5 @@\n-five\n+FIVE\n\
             @@ -1 +1 @@\n-one\n+ONE\n";
        let error = super::parse_segment(diff, 4)
            .err()
            .ok_or("out-of-order hunk ranges must fail to parse")?;
        assert!(
            error.to_string().contains("file 4:"),
            "no isolated hunk fails, so the message must name the file alone: {error}"
        );
        Ok(())
    }

    #[test]
    fn truncate_detail_backs_off_a_boundary_that_would_split_a_multi_byte_character() {
        let prefix_len = PATCH_MISMATCH_DETAIL_BYTES_MAX - 1;
        let value = format!("{}é{}", "a".repeat(prefix_len), "b".repeat(10));
        let truncated = truncate_detail(value);
        assert_eq!(truncated.len(), prefix_len);
        assert_eq!(truncated, "a".repeat(prefix_len));
    }

    #[test]
    fn file_rewrite_kind_distinguishes_modify_create_and_delete() -> TestResult {
        assert!(!RewriteKind::Create.removes_file());
        assert!(!RewriteKind::Modify.removes_file());
        assert!(RewriteKind::Delete.removes_file());
        let path = rift_core::ProjectPath::new("f.rs")?;
        let _ = FileRewrite::delete(path, "");
        Ok(())
    }

    #[test]
    fn patch_creates_a_new_file_from_dev_null() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = [
            "--- /dev/null",
            "+++ b/new.rs",
            "@@ -0,0 +1 @@",
            "+pub fn fresh() {}",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths(), vec![ProjectPath("new.rs".to_owned())]);
        let written = fs::read_to_string(directory.path().join("new.rs"))?;
        assert_eq!(written, "pub fn fresh() {}\n");
        Ok(())
    }

    #[test]
    fn patch_creates_nested_parent_directories() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = [
            "--- /dev/null",
            "+++ b/a/b/c/new.rs",
            "@@ -0,0 +1 @@",
            "+pub fn nested() {}",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("a/b/c/new.rs"))?;
        assert_eq!(written, "pub fn nested() {}\n");
        Ok(())
    }

    #[test]
    fn patch_creates_an_empty_file() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = "--- /dev/null\n+++ b/empty.rs\n".to_owned();
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("empty.rs"))?;
        assert_eq!(written, "");
        Ok(())
    }

    #[test]
    fn patch_refuses_creation_outside_the_project() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let absolute = changes
            .patch(
                &reads,
                &PatchParams {
                    patch: "--- /dev/null\n+++ b//etc/passwd\n@@ -0,0 +1 @@\n+x\n"
                        .to_owned()
                        .into(),
                },
            )
            .expect_err("an absolute creation path must error");
        assert_eq!(absolute.descriptor().code(), "invalid_request");
        let dotted = changes
            .patch(
                &reads,
                &PatchParams {
                    patch: "--- /dev/null\n+++ b/../escape.rs\n@@ -0,0 +1 @@\n+x\n"
                        .to_owned()
                        .into(),
                },
            )
            .expect_err("a dot-segment creation path must error");
        assert_eq!(dotted.descriptor().code(), "invalid_request");
        Ok(())
    }

    #[test]
    fn patch_refuses_creation_when_the_target_already_exists() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = [
            "--- /dev/null",
            "+++ b/lib.rs",
            "@@ -0,0 +1 @@",
            "+pub fn fresh() {}",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("creating a path that already exists must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::TargetExists
        );
        assert_eq!(
            preconditions[0].observed,
            PreconditionValue::Boolean { value: true }
        );
        Ok(())
    }

    #[test]
    fn patch_creation_reports_a_stat_failure_when_a_parent_segment_is_a_file() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        fs::write(directory.path().join("blocked"), "not a directory\n")?;
        let patch = [
            "--- /dev/null",
            "+++ b/blocked/inner.rs",
            "@@ -0,0 +1 @@",
            "+pub fn inner() {}",
            "",
        ]
        .join("\n");
        let error = changes
            .patch(
                &reads,
                &PatchParams {
                    patch: patch.into(),
                },
            )
            .expect_err("a non-directory parent segment must surface a stat failure");
        assert_eq!(error.descriptor().code(), "storage_failure");
        assert!(
            error.to_string().contains("operation stat"),
            "failure must name the stat operation: {error}"
        );
        Ok(())
    }

    #[test]
    fn patch_refuses_creation_when_the_hunk_carries_context() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = [
            "--- /dev/null",
            "+++ b/new.rs",
            "@@ -1 +1,2 @@",
            " existing",
            "+two",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let ChangeResult::Refused { reason, .. } = result else {
            panic!("a context line cannot exist in an empty starting file");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert!(!directory.path().join("new.rs").exists());
        Ok(())
    }

    #[test]
    fn patch_deletes_a_file_on_full_match() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\npub fn steady() {}\n")?;
        let patch = [
            "--- a/lib.rs",
            "+++ /dev/null",
            "@@ -1,2 +0,0 @@",
            "-pub fn beacon() {}",
            "-pub fn steady() {}",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths(), vec![ProjectPath("lib.rs".to_owned())]);
        assert!(!directory.path().join("lib.rs").exists());
        Ok(())
    }

    #[test]
    fn patch_refuses_deletion_on_partial_match() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\npub fn steady() {}\n")?;
        let patch = [
            "--- a/lib.rs",
            "+++ /dev/null",
            "@@ -1 +0,0 @@",
            "-pub fn beacon() {}",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("deleting only part of a file must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::SourceUnchanged
        );
        let untouched = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(untouched, "pub fn beacon() {}\npub fn steady() {}\n");
        Ok(())
    }

    #[test]
    fn patch_refuses_deletion_of_a_path_the_index_does_not_serve() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = [
            "--- a/missing.rs",
            "+++ /dev/null",
            "@@ -1 +0,0 @@",
            "-pub fn gone() {}",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("deleting a path the index does not serve must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::TargetExists
        );
        Ok(())
    }

    #[test]
    fn patch_refuses_deletion_when_disk_drifted_from_the_index() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() { changed }\n",
        )?;
        let patch = [
            "--- a/lib.rs",
            "+++ /dev/null",
            "@@ -1 +0,0 @@",
            "-pub fn beacon() {}",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("deletion must refuse when the disk has drifted from the indexed source");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::SourceUnchanged
        );
        let untouched = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(untouched, "pub fn beacon() { changed }\n");
        Ok(())
    }

    #[test]
    fn patch_refuses_deletion_when_the_hunk_context_never_matches() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = [
            "--- a/lib.rs",
            "+++ /dev/null",
            "@@ -1 +0,0 @@",
            "-pub fn vanished() {}",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("a deletion hunk whose context never matches must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        let PreconditionValue::Text { value } = &preconditions[0].expected else {
            panic!("a context mismatch reports the expected hunk text");
        };
        assert!(
            value.contains("vanished"),
            "expected side must name the unmatched hunk content: {value}"
        );
        let untouched = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(untouched, "pub fn beacon() {}\n");
        Ok(())
    }

    #[test]
    fn patch_drifted_header_still_applies_by_context() -> TestResult {
        let (directory, reads, changes) =
            fixture("a\nb\nc\npub fn beacon() {}\npub fn steady() {}\n")?;
        let patch = [
            "--- a/lib.rs",
            "+++ b/lib.rs",
            "@@ -1 +1 @@",
            "-pub fn beacon() {}",
            "+pub fn beacon() -> u8 { 7 }",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths(), vec![ProjectPath("lib.rs".to_owned())]);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(
            written, "a\nb\nc\npub fn beacon() -> u8 { 7 }\npub fn steady() {}\n",
            "the header claims line 1 but the unique match sits at line 4"
        );
        Ok(())
    }

    #[test]
    fn patch_atomicity_leaves_the_first_file_untouched_when_the_second_fails() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(directory.path().join("aid.rs"), "pub fn aid() {}\n")?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        let patch = [
            "--- a/lib.rs",
            "+++ b/lib.rs",
            "@@ -1 +1 @@",
            "-pub fn beacon() {}",
            "+pub fn beacon() -> u8 { 7 }",
            "--- a/aid.rs",
            "+++ b/aid.rs",
            "@@ -1 +1 @@",
            "-pub fn never_there() {}",
            "+pub fn aid() -> u8 { 9 }",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let ChangeResult::Refused { .. } = result else {
            panic!("a mismatched second file must refuse the whole patch");
        };
        let lib = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(
            lib, "pub fn beacon() {}\n",
            "the first file's matching hunk must not land when the second file refuses"
        );
        Ok(())
    }

    #[test]
    fn patch_applies_hunks_and_reports_the_rewrite() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\npub fn steady() {}\n")?;
        let patch = [
            "--- a/lib.rs",
            "+++ b/lib.rs",
            "@@ -1,2 +1,2 @@",
            "-pub fn beacon() {}",
            "+pub fn beacon() -> u8 { 7 }",
            " pub fn steady() {}",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.clone().into(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths(), vec![ProjectPath("lib.rs".to_owned())]);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(written, "pub fn beacon() -> u8 { 7 }\npub fn steady() {}\n");
        Ok(())
    }

    #[test]
    fn patch_refuses_drifted_context_and_touches_nothing() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = [
            "--- a/lib.rs",
            "+++ b/lib.rs",
            "@@ -1 +1 @@",
            "-pub fn vanished() {}",
            "+pub fn beacon() -> u8 { 7 }",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.clone().into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("drifted hunk context must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::SourceUnchanged
        );
        let untouched = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(untouched, "pub fn beacon() {}\n");
        Ok(())
    }

    #[test]
    fn patch_rewrites_several_files_in_one_change() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(directory.path().join("aid.rs"), "pub fn aid() {}\n")?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        let patch = [
            "--- a/lib.rs",
            "+++ b/lib.rs",
            "@@ -1 +1 @@",
            "-pub fn beacon() {}",
            "+pub fn beacon() -> u8 { 7 }",
            "--- a/aid.rs",
            "+++ b/aid.rs",
            "@@ -1 +1 @@",
            "-pub fn aid() {}",
            "+pub fn aid() -> u8 { 9 }",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.clone().into(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.files.len(), 2);
        assert!(fs::read_to_string(directory.path().join("lib.rs"))?.contains("-> u8"));
        assert!(fs::read_to_string(directory.path().join("aid.rs"))?.contains("-> u8"));
        Ok(())
    }

    #[test]
    fn patch_rejects_malformed_and_escaping_input() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let no_header = changes
            .patch(
                &reads,
                &PatchParams {
                    patch: "not a diff".to_owned().into(),
                },
            )
            .expect_err("headerless input must error");
        assert_eq!(no_header.descriptor().code(), "invalid_request");
        let escaping = changes
            .patch(
                &reads,
                &PatchParams {
                    patch: "--- a/../escape.rs\n+++ b/../escape.rs\n@@ -1 +1 @@\n-x\n+y\n"
                        .to_owned()
                        .into(),
                },
            )
            .expect_err("dot segments must error");
        assert_eq!(escaping.descriptor().code(), "invalid_request");
        Ok(())
    }

    #[test]
    fn patch_rejects_more_files_than_the_bound() -> TestResult {
        use std::fmt::Write as _;

        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let mut patch = String::new();
        for index in 0..=super::PATCH_FILES_MAX {
            let _ = writeln!(
                patch,
                "--- a/f{index}.rs\n+++ b/f{index}.rs\n@@ -1 +1 @@\n-x\n+y"
            );
        }
        let error = changes
            .patch(
                &reads,
                &PatchParams {
                    patch: patch.into(),
                },
            )
            .expect_err("a diff past the file bound must error");
        assert_eq!(error.descriptor().code(), "invalid_request");
        assert!(
            error
                .to_string()
                .contains(&format!("more than {} files", super::PATCH_FILES_MAX)),
            "message must name the bound: {error}"
        );
        Ok(())
    }

    #[test]
    fn patch_shape_violation_names_an_apply_patch_envelope() {
        assert_eq!(
            super::patch_shape_violation(
                "*** Begin Patch\n*** Update File: lib.rs\n@@\n-pub fn beacon() {}\n+pub fn beacon() { }\n*** End Patch\n"
            ),
            Some(super::PatchShapeViolation::ApplyPatchEnvelope),
            "an envelope opening must outrank every other shape violation"
        );
    }

    #[test]
    fn patch_shape_violation_names_a_crlf_apply_patch_envelope() {
        assert_eq!(
            super::patch_shape_violation(
                "*** Begin Patch\r\n*** Update File: lib.rs\r\n@@\r\n-a\r\n+b\r\n*** End Patch\r\n"
            ),
            Some(super::PatchShapeViolation::ApplyPatchEnvelope),
            "a CRLF envelope opening must classify as the envelope it is"
        );
    }

    #[test]
    fn patch_shape_violation_names_a_body_carrying_no_file_header() {
        assert_eq!(
            super::patch_shape_violation("not a diff\n"),
            Some(super::PatchShapeViolation::NoFileHeaders),
            "a body with no `--- ` line carries no file header"
        );
    }

    #[test]
    fn patch_shape_violation_names_a_file_header_no_hunk_follows() {
        assert_eq!(
            super::patch_shape_violation("--- a/lib.rs\n+++ b/lib.rs\n"),
            Some(super::PatchShapeViolation::NoHunks),
            "headers without a `@@` line carry no hunk"
        );
    }

    #[test]
    fn patch_shape_violation_counts_the_files_past_the_bound() {
        use std::fmt::Write as _;

        let mut patch = String::new();
        for index in 0..=super::PATCH_FILES_MAX {
            let _ = writeln!(
                patch,
                "--- a/f{index}.rs\n+++ b/f{index}.rs\n@@ -1 +1 @@\n-x\n+y"
            );
        }
        assert_eq!(
            super::patch_shape_violation(&patch),
            Some(super::PatchShapeViolation::TooManyFiles {
                file_count: super::PATCH_FILES_MAX + 1
            }),
            "the violation must carry the file count that crossed the bound"
        );
    }

    #[test]
    fn patch_shape_violation_accepts_a_unified_diff() {
        assert_eq!(
            super::patch_shape_violation("--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n-x\n+y\n"),
            None,
            "a diff with headers and a hunk carries no shape violation"
        );
        assert_eq!(
            super::patch_shape_violation("--- /dev/null\n+++ b/empty.rs\n"),
            None,
            "a creation carries no hunk of its own"
        );
    }

    #[test]
    fn patch_refuses_file_rename_as_unsupported() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = [
            "--- a/lib.rs",
            "+++ b/other.rs",
            "@@ -1 +1 @@",
            "-pub fn beacon() {}",
            "+pub fn beacon() -> u8 { 7 }",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let ChangeResult::Refused { reason, .. } = result else {
            panic!("file rename must refuse this release");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        let untouched = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(untouched, "pub fn beacon() {}\n");
        Ok(())
    }

    #[test]
    fn patch_refuses_a_path_the_index_does_not_serve() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = "--- a/ghost.rs\n+++ b/ghost.rs\n@@ -1 +1 @@\n-pub fn beacon() {}\n+pub fn beacon() -> u8 { 7 }\n";
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("a patch for an unindexed path must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::TargetExists
        );
        assert_eq!(
            preconditions[0].paths,
            vec![ProjectPath("ghost.rs".to_owned())]
        );
        Ok(())
    }

    #[test]
    fn patch_refuses_when_disk_drifted_from_snapshot() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() { }\n")?;
        let patch = "--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n-pub fn beacon() {}\n+pub fn beacon() -> u8 { 7 }\n";
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("a drifted file under a patch must refuse before writing");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::SourceUnchanged
        );
        assert_ne!(preconditions[0].expected, preconditions[0].observed);
        let untouched = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(untouched, "pub fn beacon() { }\n");
        Ok(())
    }

    /// Builds workspace with provider source and baseline text files.
    fn visible_file_fixture() -> TestResult<(tempfile::TempDir, ReadService, ChangeService)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(directory.path().join("notes.mdx"), "# Notes\nFirst line.\n")?;
        fs::write(
            directory.path().join("justfile"),
            "default:\n    echo hello\n",
        )?;
        fs::write(directory.path().join(".gitignore"), "target\n")?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        Ok((directory, reads, changes))
    }

    #[test]
    fn patch_applies_against_a_text_indexed_file_no_syntax_provider_parses() -> TestResult {
        let (directory, reads, changes) = visible_file_fixture()?;
        let patch = "--- a/notes.mdx\n+++ b/notes.mdx\n@@ -1,2 +1,2 @@\n # Notes\n-First line.\n+Updated line.\n".to_owned();
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths(), vec![ProjectPath("notes.mdx".to_owned())]);
        let written = fs::read_to_string(directory.path().join("notes.mdx"))?;
        assert_eq!(written, "# Notes\nUpdated line.\n");
        Ok(())
    }

    /// Baseline files no syntax provider claims still resolve as patch targets.
    #[test]
    fn patch_applies_against_baseline_files_without_syntax_providers() -> TestResult {
        let (directory, reads, changes) = visible_file_fixture()?;
        let patch = [
            "--- a/justfile",
            "+++ b/justfile",
            "@@ -1,2 +1,2 @@",
            " default:",
            "-    echo hello",
            "+    echo goodbye",
            "--- a/.gitignore",
            "+++ b/.gitignore",
            "@@ -1 +1,2 @@",
            " target",
            "+build",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths().len(), 2);
        assert_eq!(
            fs::read_to_string(directory.path().join("justfile"))?,
            "default:\n    echo goodbye\n"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join(".gitignore"))?,
            "target\nbuild\n"
        );
        Ok(())
    }

    #[test]
    fn patch_deletes_a_file_no_extension_gate_admits() -> TestResult {
        let (directory, reads, changes) = visible_file_fixture()?;
        let patch = [
            "--- a/justfile",
            "+++ /dev/null",
            "@@ -1,2 +0,0 @@",
            "-default:",
            "-    echo hello",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths(), vec![ProjectPath("justfile".to_owned())]);
        assert!(!directory.path().join("justfile").exists());
        Ok(())
    }

    #[test]
    fn patch_against_a_source_excluded_path_refuses_unsupported_not_target_exists() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        fs::write(
            directory.path().join("justfile"),
            "default:\n    echo hello\n",
        )?;
        let visibility = SourceVisibility::new(Vec::new(), vec!["justfile".to_owned()], true);
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        let patch = "--- a/justfile\n+++ b/justfile\n@@ -1,2 +1,2 @@\n default:\n-    echo hello\n+    echo goodbye\n".to_owned();
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            diagnostics,
        } = result
        else {
            panic!("a [source]-excluded target must refuse");
        };
        assert_eq!(
            reason,
            RefusalReason::Unsupported,
            "a policy-excluded target refuses as unsupported, not unmet_precondition"
        );
        assert!(
            preconditions.is_empty(),
            "an unsupported refusal carries no preconditions: {preconditions:?}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("justfile")
                    && diagnostic.message.contains("[source]")),
            "the diagnostic must name the excluded path and the policy: {diagnostics:?}"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("justfile"))?,
            "default:\n    echo hello\n",
            "an excluded target is untouched"
        );
        Ok(())
    }

    /// `resolve_create` consults the same `[source]` policy a modify already
    /// does, rather than reaching the filesystem unconditionally.
    #[test]
    fn patch_create_into_an_excluded_directory_refuses_unsupported() -> TestResult {
        let directory = tempfile::tempdir()?;
        let visibility = SourceVisibility::new(Vec::new(), vec!["excluded/**".to_owned()], true);
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &visibility,
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        let patch =
            "--- /dev/null\n+++ b/excluded/new.rs\n@@ -0,0 +1 @@\n+pub fn fresh() {}\n".to_owned();
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = result
        else {
            panic!("a create into an excluded directory must refuse");
        };
        assert_eq!(
            reason,
            RefusalReason::Unsupported,
            "a policy-excluded create refuses as unsupported, not unmet_precondition"
        );
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic.message.contains("excluded/new.rs")
                    && diagnostic.message.contains("[source]")
            }),
            "the diagnostic must name the excluded path and the policy: {diagnostics:?}"
        );
        assert!(
            !directory.path().join("excluded").exists(),
            "a refused create must leave the tree untouched"
        );
        Ok(())
    }

    /// `report.rs` and `report.log` share a stem: staging both through
    /// `Path::with_extension` used to collide on one `report.rift-staged`
    /// path, so the second write silently destroyed the first file's
    /// bytes. The exclusive tempfile staging closes this.
    #[test]
    fn patch_applies_two_files_sharing_a_stem_without_collision() -> TestResult {
        let directory = tempfile::tempdir()?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        let patch = [
            "--- /dev/null",
            "+++ b/report.rs",
            "@@ -0,0 +1 @@",
            "+pub fn one() {}",
            "--- /dev/null",
            "+++ b/report.log",
            "@@ -0,0 +1 @@",
            "+report line",
            "",
        ]
        .join("\n");
        let summary = applied_summary(changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?);
        assert_eq!(summary.paths().len(), 2);
        assert_eq!(
            fs::read_to_string(directory.path().join("report.rs"))?,
            "pub fn one() {}\n",
            "report.rs must hold its own content, not report.log's"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("report.log"))?,
            "report line\n",
            "report.log must hold its own content, not report.rs's"
        );
        Ok(())
    }

    /// A patch through an in-workspace symlink leaves the link a link and
    /// writes through to its resolved target, with the result naming both
    /// paths.
    #[cfg(unix)]
    #[test]
    fn patch_through_an_in_workspace_symlink_updates_the_resolved_target_and_warns() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("real.rs"), "pub fn beacon() {}\n")?;
        std::os::unix::fs::symlink("real.rs", directory.path().join("link.rs"))?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        let patch =
            "--- a/link.rs\n+++ b/link.rs\n@@ -1 +1 @@\n-pub fn beacon() {}\n+pub fn renamed() {}\n"
                .to_owned();
        let summary = applied_summary(changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?);
        assert_eq!(summary.paths(), vec![ProjectPath("link.rs".to_owned())]);
        assert!(
            fs::symlink_metadata(directory.path().join("link.rs"))?
                .file_type()
                .is_symlink(),
            "the link itself must remain a symlink"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("real.rs"))?,
            "pub fn renamed() {}\n",
            "the resolved target's bytes must update"
        );
        assert!(
            summary
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("link.rs")
                    && diagnostic.message.contains("real.rs")),
            "the result must carry a warning naming both paths: {:?}",
            summary.diagnostics
        );
        Ok(())
    }

    /// A symlink whose target lies outside the workspace refuses before
    /// staging, naming the link, and leaves both the link and the outside
    /// file untouched.
    #[cfg(unix)]
    #[test]
    fn patch_through_a_symlink_outside_the_workspace_refuses_and_leaves_both_untouched()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        fs::write(outside.path().join("secret.rs"), "pub fn secret() {}\n")?;
        std::os::unix::fs::symlink(
            outside.path().join("secret.rs"),
            directory.path().join("link.rs"),
        )?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        let patch =
            "--- a/link.rs\n+++ b/link.rs\n@@ -1 +1 @@\n-pub fn secret() {}\n+pub fn exposed() {}\n"
                .to_owned();
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = result
        else {
            panic!("a symlink resolving outside the workspace must refuse");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("link.rs")),
            "{diagnostics:?}"
        );
        assert!(
            fs::symlink_metadata(directory.path().join("link.rs"))?
                .file_type()
                .is_symlink(),
            "the link itself must remain untouched"
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("secret.rs"))?,
            "pub fn secret() {}\n",
            "the outside file must remain untouched"
        );
        Ok(())
    }

    #[test]
    fn patch_with_crlf_endings_applies_and_preserves_them() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\r\npub fn steady() {}\r\n")?;
        let patch = [
            "--- a/lib.rs",
            "+++ b/lib.rs",
            "@@ -1,2 +1,2 @@",
            "-pub fn beacon() {}",
            "+pub fn beacon() -> u8 { 7 }",
            " pub fn steady() {}",
            "",
        ]
        .join("\r\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths(), vec![ProjectPath("lib.rs".to_owned())]);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(
            written, "pub fn beacon() -> u8 { 7 }\r\npub fn steady() {}\r\n",
            "CRLF endings must survive the rewrite byte-for-byte"
        );
        Ok(())
    }

    /// An LF diff locates against CRLF source by content alone, the way `git apply`
    /// tolerates it, and the rewritten line adopts the source's own CRLF ending.
    #[test]
    fn patch_with_lf_context_applies_against_crlf_source_and_adopts_its_ending() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\r\n")?;
        let patch = [
            "--- a/lib.rs",
            "+++ b/lib.rs",
            "@@ -1 +1 @@",
            "-pub fn beacon() {}",
            "+pub fn beacon() -> u8 { 7 }",
            "",
        ]
        .join("\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths(), vec![ProjectPath("lib.rs".to_owned())]);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(written, "pub fn beacon() -> u8 { 7 }\r\n");
        Ok(())
    }

    /// The reverse direction: a CRLF diff locates against LF source, and the rewritten
    /// line adopts the source's own LF ending.
    #[test]
    fn patch_with_crlf_context_applies_against_lf_source_and_adopts_its_ending() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = [
            "--- a/lib.rs",
            "+++ b/lib.rs",
            "@@ -1 +1 @@",
            "-pub fn beacon() {}",
            "+pub fn beacon() -> u8 { 7 }",
            "",
        ]
        .join("\r\n");
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths(), vec![ProjectPath("lib.rs".to_owned())]);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(written, "pub fn beacon() -> u8 { 7 }\n");
        Ok(())
    }

    /// A file whose lines carry different endings: the changed line adopts its own
    /// position's prevailing ending, and every untouched line keeps its own, unaltered.
    #[test]
    fn patch_against_mixed_ending_source_keeps_each_untouched_lines_own_ending() -> TestResult {
        let (directory, reads, changes) =
            fixture("pub fn one() {}\r\npub fn two() {}\npub fn three() {}\r\n")?;
        let patch = "--- a/lib.rs\n+++ b/lib.rs\n@@ -2 +2 @@\n-pub fn two() {}\n+pub fn TWO() {}\n";
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.to_owned().into(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths(), vec![ProjectPath("lib.rs".to_owned())]);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(
            written, "pub fn one() {}\r\npub fn TWO() {}\npub fn three() {}\r\n",
            "the changed line takes line 2's own LF ending; lines 1 and 3 keep their CRLF \
             untouched"
        );
        Ok(())
    }

    #[test]
    fn patch_against_a_directory_target_refuses_target_is_file() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        fs::create_dir(directory.path().join("adir"))?;
        let patch = "--- a/adir\n+++ b/adir\n@@ -1 +1 @@\n-x\n+y\n";
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.to_owned().into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("patching a directory must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::TargetIsFile
        );
        assert_eq!(
            preconditions[0].expected,
            PreconditionValue::Boolean { value: true }
        );
        assert_eq!(
            preconditions[0].observed,
            PreconditionValue::Boolean { value: false }
        );
        assert!(
            directory.path().join("adir").is_dir(),
            "the directory is untouched"
        );
        Ok(())
    }

    #[test]
    fn patch_creation_into_a_directory_refuses_target_is_file() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        fs::create_dir(directory.path().join("adir"))?;
        let patch = "--- /dev/null\n+++ b/adir\n@@ -0,0 +1 @@\n+pub fn fresh() {}\n";
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.to_owned().into(),
            },
        )?;
        let ChangeResult::Refused {
            reason,
            preconditions,
            ..
        } = result
        else {
            panic!("creating over a directory must refuse");
        };
        assert_eq!(reason, RefusalReason::UnmetPrecondition);
        assert_eq!(
            preconditions[0].kind,
            OperationPreconditionKind::TargetIsFile
        );
        assert!(
            directory.path().join("adir").is_dir(),
            "the directory is untouched"
        );
        Ok(())
    }

    /// A file `resolve_base_image` finds absent from index reads through `[source]` policy
    /// directly, and stat failure on that read surfaces as `storage_failure` rather
    /// than reaching the mismatch path.
    #[cfg(unix)]
    #[test]
    fn patch_against_an_unreadable_source_directory_is_a_storage_failure() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir()?;
        let sealed = directory.path().join("sealed");
        fs::create_dir(&sealed)?;
        fs::write(sealed.join("Cargo.lock"), "# generated\n")?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let changes = ChangeService::new(directory.path());
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000))?;
        let patch = "--- a/sealed/Cargo.lock\n+++ b/sealed/Cargo.lock\n@@ -1 +1 @@\n-# generated\n+# regenerated\n";
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.to_owned().into(),
            },
        );
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755))?;
        let error = result.expect_err("an unreadable source directory is a storage failure");
        assert_eq!(error.descriptor().code(), "storage_failure");
        Ok(())
    }

    /// `resolve_create` asks [`crate::publish::resolve_write_target`] first, which stats
    /// the same path and already turns a stat failure into `storage_failure` before
    /// `creation_conflict` runs - so this drives `creation_conflict` directly, the only
    /// way to prove its own stat-error arm rather than the one upstream of it.
    #[cfg(unix)]
    #[test]
    fn creation_conflict_on_an_unreadable_directory_is_a_storage_failure() -> TestResult {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir()?;
        let sealed = directory.path().join("sealed");
        fs::create_dir(&sealed)?;
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000))?;
        let path = CoreProjectPath::new("sealed/fresh.rs").expect("fixture path validates");
        let result = creation_conflict(directory.path(), &path);
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o755))?;
        let error = result.expect_err("an unreadable directory is a storage failure");
        assert_eq!(error.descriptor().code(), "storage_failure");
        Ok(())
    }

    #[test]
    fn patch_without_a_trailing_newline_applies() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = "--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n-pub fn beacon() {}\n+pub fn beacon() -> u8 { 7 }";
        let result = changes.patch(
            &reads,
            &PatchParams {
                patch: patch.into(),
            },
        )?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths(), vec![ProjectPath("lib.rs".to_owned())]);
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert_eq!(written, "pub fn beacon() -> u8 { 7 }");
        Ok(())
    }

    #[test]
    fn patch_with_backslash_paths_names_the_expected_form() -> TestResult {
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = "--- \"a\\\\lib.rs\"\n+++ \"b\\\\lib.rs\"\n@@ -1 +1 @@\n-pub fn beacon() {}\n+pub fn beacon() -> u8 { 7 }\n";
        let error = changes
            .patch(
                &reads,
                &PatchParams {
                    patch: patch.into(),
                },
            )
            .expect_err("backslash separators must fail as an invalid path");
        assert_eq!(error.descriptor().code(), "invalid_request");
        assert!(
            error.to_string().contains("forward-slash"),
            "message must name the expected form: {error}"
        );
        Ok(())
    }

    /// `patch` with `{"file": ...}` applies identically to the same diff sent inline.
    #[test]
    fn patch_file_form_matches_the_inline_form_byte_identically() -> TestResult {
        let diff = "--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n-pub fn beacon() {}\n+pub fn beacon() -> u8 { 3 }\n";
        let scratch = tempfile::tempdir()?;
        let scratch_file = scratch.path().join("change.diff");
        fs::write(&scratch_file, diff)?;

        let (inline_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        applied_summary(changes.patch(&reads, &PatchParams { patch: diff.into() })?);

        let (file_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        applied_summary(changes.patch(
            &reads,
            &PatchParams {
                patch: BodySource::File {
                    file: scratch_file.to_string_lossy().into_owned(),
                },
            },
        )?);

        assert_eq!(
            fs::read(inline_directory.path().join("lib.rs"))?,
            fs::read(file_directory.path().join("lib.rs"))?,
            "the inline and file forms must write byte-identical trees"
        );
        Ok(())
    }

    /// A `patch` at [`PATCH_BYTES_MAX`] applies; one byte over refuses `unsupported`
    /// naming the byte count.
    #[test]
    fn patch_bound_accepts_the_limit_and_refuses_one_byte_over() -> TestResult {
        // The bound is enforced before the patch is parsed, so this need not be a
        // legal diff: it only needs to be one byte longer than PATCH_BYTES_MAX.
        let over = "x".repeat(PATCH_BYTES_MAX + 1);
        let (_directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let result = changes.patch(&reads, &PatchParams { patch: over.into() })?;
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = result
        else {
            panic!("a patch one byte over the bound must refuse");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert!(
            diagnostics[0]
                .message
                .contains(&(PATCH_BYTES_MAX + 1).to_string())
        );
        Ok(())
    }

    /// A one-line patch against a file already over [`REWRITE_FILE_BYTES_MAX`]
    /// refuses: nothing bounds the patch text itself here, since the change is tiny,
    /// so the shared rewrite-result check in `publish_rewrites` is what catches it.
    #[test]
    fn patch_against_an_already_oversized_file_refuses() -> TestResult {
        let existing = format!(
            "pub fn beacon() {{}}\n// {}\n",
            "x".repeat(REWRITE_FILE_BYTES_MAX)
        );
        let (directory, reads, changes) = fixture(&existing)?;
        let diff = "--- a/lib.rs\n+++ b/lib.rs\n@@ -1 +1 @@\n-pub fn beacon() {}\n+pub fn beacon() -> u8 { 1 }\n";
        let result = changes.patch(&reads, &PatchParams { patch: diff.into() })?;
        let ChangeResult::Refused { reason, .. } = result else {
            panic!("a patch against an already oversized file must refuse");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert_eq!(
            fs::read_to_string(directory.path().join("lib.rs"))?,
            existing,
            "a refused patch leaves the tree untouched"
        );
        Ok(())
    }

    #[test]
    fn apply_segment_replaces_the_located_run() -> TestResult {
        let diff = "--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let applied = apply_segment("alpha\nbeta\ngamma\n", &parsed)
            .map_err(|_| "the full context+delete run must be located")?;
        assert_eq!(applied.next_source, "alpha\nBETA\ngamma\n");
        Ok(())
    }

    #[test]
    fn apply_segment_applies_an_insertion_only_hunk() -> TestResult {
        let diff = "--- a/f\n+++ b/f\n@@ -1,0 +2,1 @@\n+NEW\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let applied =
            apply_segment("one\ntwo\n", &parsed).map_err(|_| "a pure insertion hunk must apply")?;
        assert_eq!(applied.next_source, "one\nNEW\ntwo\n");
        Ok(())
    }

    #[test]
    fn apply_segment_applies_a_deletion_only_hunk() -> TestResult {
        let diff = "--- a/f\n+++ b/f\n@@ -2 +1,0 @@\n-two\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let applied = apply_segment("one\ntwo\nthree\n", &parsed)
            .map_err(|_| "a deletion-only hunk must apply")?;
        assert_eq!(applied.next_source, "one\nthree\n");
        Ok(())
    }

    #[test]
    fn apply_segment_applies_a_hunk_the_backward_search_locates() -> TestResult {
        // "P" sits at line 2 and "Q" at line 9; the first hunk claims and finds
        // "Q" directly, but the second hunk's header claims line 10 while "P"
        // only exists at line 2 - the backward search that locates it lands
        // before the first hunk's own run.
        let diff = "--- a/f\n+++ b/f\n\
             @@ -9,1 +9,1 @@\n-Q\n+QQ\n\
             @@ -10,1 +10,1 @@\n-P\n+PP\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let source = "x\nP\nx\nx\nx\nx\nx\nx\nQ\nx\nx\n";
        let applied = apply_segment(source, &parsed).map_err(
            |_| "the second hunk must still be found after searching backward past the first",
        )?;
        assert_eq!(applied.next_source, "x\nPP\nx\nx\nx\nx\nx\nx\nQQ\nx\nx\n");
        Ok(())
    }

    #[test]
    fn apply_segment_applies_a_trailing_insertion_after_the_tail_was_replaced() -> TestResult {
        // The first hunk replaces both remaining lines of a 3-line file; the
        // second hunk then inserts after that point, where nothing original
        // is left standing.
        let diff = "--- a/f\n+++ b/f\n@@ -2,2 +2,1 @@\n-b\n-c\n+BC\n@@ -4,0 +3,1 @@\n+TAIL\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let applied = apply_segment("a\nb\nc\n", &parsed)
            .map_err(|_| "a tail-replacing hunk followed by a trailing insertion must apply")?;
        assert_eq!(applied.next_source, "a\nBC\nTAIL\n");
        Ok(())
    }

    #[test]
    fn hunk_counts_of_counts_each_line_kind_correctly() {
        assert_eq!(
            HunkCounts::of(&[" ctx\n"]),
            HunkCounts { old: 1, new: 1 },
            "a space-prefixed line counts on both sides"
        );
        assert_eq!(
            HunkCounts::of(&["-old\n"]),
            HunkCounts { old: 1, new: 0 },
            "a `-` line counts only the old side"
        );
        assert_eq!(
            HunkCounts::of(&["+new\n"]),
            HunkCounts { old: 0, new: 1 },
            "a `+` line counts only the new side"
        );
        assert_eq!(
            HunkCounts::of(&["\n"]),
            HunkCounts { old: 1, new: 1 },
            "a bare empty line counts on both sides"
        );
        assert_eq!(
            HunkCounts::of(&["\\ No newline at end of file\n"]),
            HunkCounts { old: 0, new: 0 },
            "the no-newline marker counts on neither side"
        );
    }

    #[test]
    fn rewritten_header_refuses_a_malformed_header_shape() {
        let counts = HunkCounts { old: 1, new: 1 };
        assert!(
            rewritten_header("not a header\n", counts).is_none(),
            "a line that does not open with `@@` must not be rewritten"
        );
        assert!(
            rewritten_header("@@ -1,2 @@\n", counts).is_none(),
            "a header missing its `+new` side must not be rewritten"
        );
        assert!(
            rewritten_header("@@ -x,2 +1,3 @@\n", counts).is_none(),
            "a header whose offset is not numeric must not be rewritten"
        );
        assert!(
            rewritten_header("@@ -1,2 +1,3 +9,9 @@\n", counts).is_none(),
            "a header carrying a third range must not be rewritten"
        );
    }

    #[test]
    fn rewritten_header_keeps_offsets_trailing_heading_and_line_ending() {
        let header = "@@ -10,3 +12,5 @@ fn heading() {\n";
        let counts = HunkCounts { old: 7, new: 2 };
        let rewritten =
            rewritten_header(header, counts).expect("a well-formed header must rewrite");
        assert_eq!(rewritten, "@@ -10,7 +12,2 @@ fn heading() {\n");
    }

    #[test]
    fn apply_segment_applies_a_miscounted_header_identically_to_a_correct_one() -> TestResult {
        let source = "line1\nline2\nline3\nline4\nline5\n";
        let body = " line1\n-line2\n-line3\n+NEW2\n+NEW3\n line4\n";
        let wrong = format!("--- a/f\n+++ b/f\n@@ -1,11 +1,6 @@\n{body}");
        let correct = format!("--- a/f\n+++ b/f\n@@ -1,4 +1,4 @@\n{body}");
        let wrong_segments = hunks(&wrong)?;
        let correct_segments = hunks(&correct)?;
        assert_eq!(
            wrong_segments[0], correct_segments[0],
            "recounting from the body must erase the header's originally wrong counts entirely"
        );
        let wrong_parsed = Patch::from_str(&wrong_segments[0])?;
        let correct_parsed = Patch::from_str(&correct_segments[0])?;
        let wrong_applied = apply_segment(source, &wrong_parsed)
            .map_err(|_| "a miscounted header must still apply once recounted")?;
        let correct_applied = apply_segment(source, &correct_parsed)
            .map_err(|_| "the correctly counted header must apply")?;
        assert_eq!(wrong_applied.next_source, correct_applied.next_source);
        assert_eq!(
            wrong_applied.next_source,
            "line1\nNEW2\nNEW3\nline4\nline5\n"
        );
        Ok(())
    }

    #[test]
    fn apply_segment_applies_a_hunk_whose_header_omits_the_count() -> TestResult {
        let diff = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-one\n+ONE\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let applied = apply_segment("one\n", &parsed)
            .map_err(|_| "a header with an omitted count must still parse and apply")?;
        assert_eq!(applied.next_source, "ONE\n");
        Ok(())
    }

    #[test]
    fn apply_segment_refuses_when_the_recounted_header_still_cannot_locate_its_context()
    -> TestResult {
        let diff = "--- a/f\n+++ b/f\n@@ -1,99 +1,99 @@\n-vanished\n+VANISHED\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let detail = apply_segment("one\ntwo\n", &parsed)
            .err()
            .ok_or("wrong counts plus unmatchable content must still refuse")?;
        assert_eq!(detail.ordinal, 1);
        assert_eq!(detail.expected, "vanished (LF ending)");
        assert_eq!(detail.observed, "one (LF ending)");
        assert!(
            detail.header.contains("@@ -1 +1 @@"),
            "the refusal reports the recounted header (one line each side), not the original \
             wrong count of 99: {}",
            detail.header
        );
        Ok(())
    }

    #[test]
    fn apply_segment_keeps_crlf_bytes_through_the_recounted_header_and_applies() -> TestResult {
        let diff = "--- a/f\r\n+++ b/f\r\n@@ -1,2 +1,2 @@\r\n-one\r\n+ONE\r\n two\r\n";
        let segments = hunks(diff)?;
        assert!(
            segments[0].contains("-one\r\n"),
            "hunk body bytes, CRLF included, must survive normalization untouched: {:?}",
            segments[0]
        );
        let parsed = Patch::from_str(&segments[0])?;
        let applied = apply_segment("one\r\ntwo\r\n", &parsed)
            .map_err(|_| "a CRLF hunk body must match a CRLF source")?;
        assert_eq!(applied.next_source, "ONE\r\ntwo\r\n");
        Ok(())
    }

    #[test]
    fn named_parse_error_names_the_hunk_whose_header_is_malformed() -> TestResult {
        let diff = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-one\n+ONE\n@@ nonsense @@\n-two\n+TWO\n";
        let segments = hunks(diff)?;
        let error = super::parse_segment(&segments[0], 5)
            .err()
            .ok_or("a malformed second hunk header must fail to parse")?;
        assert!(
            error.to_string().contains("file 5 hunk 2"),
            "message must name the file and the malformed hunk: {error}"
        );
        Ok(())
    }
}
