//! Unified-diff patch engine: parses hunks with `diffy`, then locates and
//! applies each one itself, following `git apply` semantics.
//!
//! Header line numbers are a hint, not a requirement: context and deleted
//! lines must match exactly, but the position they match at may drift from
//! the header. Each hunk's search starts at its header position corrected
//! by the drift already discovered from earlier hunks in the same file —
//! the running delta `git apply` itself carries forward — then widens
//! outward, nearer positions first and the earlier position on a tie.
//! `/dev/null` headers create or delete a file; renames and copies stay
//! unsupported.

use std::fs;
use std::path::Path;

use diffy::{Hunk, HunkRange, Line, ParsePatchError, Patch};
use rift_core::ProjectPath as CoreProjectPath;
use rift_protocol::change::{
    ChangeResult, OperationPrecondition, OperationPreconditionKind, OperationPreconditionStatus,
    PreconditionValue, RefusalReason,
};

use crate::change::digest_hex8;
use crate::read::{ReadError, ReadFault, ReadService};

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

/// How one resolved file segment changes the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RewriteKind {
    /// An existing file's content changes in place.
    Modify,
    /// A new file is written; its parent directories are created first.
    Create,
    /// An existing file is removed.
    Delete,
}

/// One file-level effect a patch resolved to, not yet written.
#[derive(Debug)]
pub(crate) struct FileRewrite {
    pub(crate) path: CoreProjectPath,
    pub(crate) kind: RewriteKind,
    pub(crate) previous_len: u64,
    pub(crate) next_source: String,
}

/// Splits one unified diff into its per-file segments. Only a header line
/// opens a segment: hunk body lines never start with `---` at column zero,
/// because context lines carry a leading space and removals a single `-`.
///
/// Hunk body bytes pass through untouched — a CRLF patch keeps its `\r`
/// so its context matches a CRLF source, and an ending mismatch surfaces
/// as hunk-context drift, never a silent rewrite. Only structural lines
/// shed a CRLF ending, because the diff parser rejects `\r` in headers.
pub(crate) fn split_file_segments(patch: &str) -> Result<Vec<String>, ReadError> {
    let mut segments: Vec<String> = Vec::new();
    let mut segment_line = 0_usize;
    for line in patch.split_inclusive('\n') {
        if line.starts_with(ORIGINAL_HEADER_PREFIX) {
            if segments.len() == PATCH_FILES_MAX {
                return Err(ReadFault::invalid(
                    "patch",
                    format!("addresses more than {PATCH_FILES_MAX} files"),
                ));
            }
            segments.push(String::new());
            segment_line = 0;
        }
        if let Some(segment) = segments.last_mut() {
            let structural = segment_line == 0
                || (segment_line == 1 && line.starts_with(MODIFIED_HEADER_PREFIX))
                || line.starts_with(HUNK_HEADER_PREFIX);
            push_segment_line(segment, line, structural);
            segment_line += 1;
        }
    }
    if segments.is_empty() {
        return Err(ReadFault::invalid("patch", "carries no `---` file header"));
    }
    Ok(segments)
}

/// Appends one diff line to its segment. A structural line — the `---` and
/// `+++` file headers and `@@` hunk headers — sheds a CRLF ending, because
/// the diff parser rejects `\r` there; body lines keep their exact bytes so
/// hunk content matches the stored source byte-for-byte.
fn push_segment_line(segment: &mut String, line: &str, structural: bool) {
    match (structural, line.strip_suffix("\r\n")) {
        (true, Some(stripped)) => {
            segment.push_str(stripped);
            segment.push('\n');
        }
        _ => segment.push_str(line),
    }
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
        Ok(PatchTarget::Create(path)) => resolve_create(root, &path, &parsed),
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
    let mut header = String::new();
    let mut hunks: Vec<String> = Vec::new();
    for line in segment.split_inclusive('\n') {
        if line.starts_with(HUNK_HEADER_PREFIX) {
            hunks.push(String::new());
        }
        match hunks.last_mut() {
            Some(hunk) => hunk.push_str(line),
            None => header.push_str(line),
        }
    }
    (header, hunks)
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

/// Resolves a segment that edits an existing file.
fn resolve_modify(
    root: &Path,
    reads: &ReadService,
    path: &CoreProjectPath,
    parsed: &Patch<'_, str>,
) -> Result<Result<FileRewrite, ChangeResult>, ReadError> {
    let Some(file) = reads.index().file(path) else {
        return Ok(Err(target_missing_refusal(path)));
    };
    let disk = fs::read_to_string(root.join(path.as_str()))
        .map_err(|error| ReadFault::storage(path.as_str(), "read", &error))?;
    if disk != file.source() {
        return Ok(Err(source_drift_refusal(path, file.source(), &disk)));
    }
    match apply_segment(file.source(), parsed) {
        Ok(next_source) => Ok(Ok(FileRewrite {
            path: path.clone(),
            kind: RewriteKind::Modify,
            previous_len: file.source().len() as u64,
            next_source,
        })),
        Err(detail) => Ok(Err(detail.into_refusal(path))),
    }
}

/// Resolves a segment that creates a new file. The starting image is
/// empty, so a hunk carrying context or deleted lines cannot locate a
/// position and refuses exactly as a genuine mismatch would.
fn resolve_create(
    root: &Path,
    path: &CoreProjectPath,
    parsed: &Patch<'_, str>,
) -> Result<Result<FileRewrite, ChangeResult>, ReadError> {
    match root.join(path.as_str()).try_exists() {
        Ok(true) => return Ok(Err(already_exists_refusal(path))),
        Ok(false) => {}
        Err(error) => return Err(ReadFault::storage(path.as_str(), "stat", &error)),
    }
    match apply_segment("", parsed) {
        Ok(next_source) => Ok(Ok(FileRewrite {
            path: path.clone(),
            kind: RewriteKind::Create,
            previous_len: 0,
            next_source,
        })),
        Err(detail) => Ok(Err(detail.into_refusal(path))),
    }
}

/// Resolves a segment that deletes an existing file: the hunks must
/// consume the entire file, leaving nothing behind.
fn resolve_delete(
    root: &Path,
    reads: &ReadService,
    path: &CoreProjectPath,
    parsed: &Patch<'_, str>,
) -> Result<Result<FileRewrite, ChangeResult>, ReadError> {
    let Some(file) = reads.index().file(path) else {
        return Ok(Err(target_missing_refusal(path)));
    };
    let disk = fs::read_to_string(root.join(path.as_str()))
        .map_err(|error| ReadFault::storage(path.as_str(), "read", &error))?;
    if disk != file.source() {
        return Ok(Err(source_drift_refusal(path, file.source(), &disk)));
    }
    match apply_segment(file.source(), parsed) {
        Ok(next_source) if next_source.is_empty() => Ok(Ok(FileRewrite {
            path: path.clone(),
            kind: RewriteKind::Delete,
            previous_len: file.source().len() as u64,
            next_source: String::new(),
        })),
        Ok(next_source) => Ok(Err(deletion_incomplete_refusal(path, &next_source))),
        Err(detail) => Ok(Err(detail.into_refusal(path))),
    }
}

/// One line of the file image a patch's hunks apply against: bytes still
/// from the original file, or bytes a previous hunk in this same run
/// already wrote. A later hunk never matches into a `Patched` line — that
/// would apply on top of text this same patch just introduced.
#[derive(Clone, Copy)]
enum ImageLine<'a> {
    Original(&'a str),
    Patched(&'a str),
}

impl<'a> ImageLine<'a> {
    fn text(self) -> &'a str {
        match self {
            Self::Original(text) | Self::Patched(text) => text,
        }
    }

    fn is_patched(self) -> bool {
        matches!(self, Self::Patched(_))
    }
}

/// Applies every hunk in `parsed` against `starting`, in order.
///
/// Each hunk's search anchors at its header position, corrected by the
/// drift already found while applying earlier hunks in this same segment.
/// The search itself never fuzzy-matches content: only exact line
/// equality locates a hunk, at a position that may differ from the
/// header. Bounded: one hunk visits at most `image.len() + 1` candidate
/// positions, since the search distance never usefully exceeds the
/// image's length.
fn apply_segment(starting: &str, parsed: &Patch<'_, str>) -> Result<String, MismatchDetail> {
    let mut image: Vec<ImageLine<'_>> = starting
        .split_inclusive('\n')
        .map(ImageLine::Original)
        .collect();
    let mut delta: i64 = 0;
    for (index, hunk) in parsed.hunks().iter().enumerate() {
        let pre_image = pre_image_lines(hunk);
        let post_image = post_image_lines(hunk);
        let expected = zero_based_start(hunk.new_range());
        let anchor = clamp_anchor(expected, delta, image.len());
        let Some(found) = find_hunk_position(&image, &pre_image, anchor) else {
            return Err(mismatch_detail(index + 1, hunk, anchor, &pre_image, &image));
        };
        delta = i64::try_from(found).unwrap_or(0) - i64::try_from(expected).unwrap_or(0);
        let patched: Vec<ImageLine<'_>> =
            post_image.iter().copied().map(ImageLine::Patched).collect();
        image.splice(found..found + pre_image.len(), patched);
    }
    Ok(image.iter().copied().map(ImageLine::text).collect())
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

/// Tests whether `pre_image` matches the image exactly at `pos`: every
/// line byte-equal, and none of them already written by an earlier hunk.
fn matches_at(image: &[ImageLine<'_>], pre_image: &[&str], pos: usize) -> bool {
    let Some(window) = image.get(pos..pos + pre_image.len()) else {
        return false;
    };
    if window.iter().copied().any(ImageLine::is_patched) {
        return false;
    }
    window
        .iter()
        .copied()
        .map(ImageLine::text)
        .eq(pre_image.iter().copied())
}

fn pre_image_lines<'a>(hunk: &Hunk<'a, str>) -> Vec<&'a str> {
    hunk.lines()
        .iter()
        .filter_map(|line| match line {
            Line::Context(text) | Line::Delete(text) => Some(*text),
            Line::Insert(_) => None,
        })
        .collect()
}

fn post_image_lines<'a>(hunk: &Hunk<'a, str>) -> Vec<&'a str> {
    hunk.lines()
        .iter()
        .filter_map(|line| match line {
            Line::Context(text) | Line::Insert(text) => Some(*text),
            Line::Delete(_) => None,
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

fn without_line_ending(line: &str) -> &str {
    line.strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(line)
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

fn mismatch_detail(
    ordinal: usize,
    hunk: &Hunk<'_, str>,
    anchor: usize,
    pre_image: &[&str],
    image: &[ImageLine<'_>],
) -> MismatchDetail {
    let expected = pre_image.first().copied().unwrap_or_default();
    let observed = image.get(anchor).copied().map(ImageLine::text);
    MismatchDetail {
        ordinal,
        header: hunk_header_text(hunk),
        line: anchor + 1,
        expected: without_line_ending(expected).to_owned(),
        observed: observed.map_or_else(
            || "end of file".to_owned(),
            |text| without_line_ending(text).to_owned(),
        ),
    }
}

impl MismatchDetail {
    fn into_refusal(self, path: &CoreProjectPath) -> ChangeResult {
        ChangeResult::refused(
            RefusalReason::UnmetPrecondition,
            vec![OperationPrecondition::new(
                OperationPreconditionKind::SourceUnchanged,
                OperationPreconditionStatus::Failed,
                Vec::new(),
                vec![path.as_str().to_owned()],
                PreconditionValue::Text {
                    value: truncate_detail(self.side_text("expected", &self.expected)),
                },
                PreconditionValue::Text {
                    value: truncate_detail(self.side_text("found", &self.observed)),
                },
            )],
        )
    }

    fn side_text(&self, label: &str, content: &str) -> String {
        format!(
            "hunk {} `{}`, line {}: {label} `{content}`",
            self.ordinal, self.header, self.line
        )
    }
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

fn target_missing_refusal(path: &CoreProjectPath) -> ChangeResult {
    ChangeResult::refused(
        RefusalReason::UnmetPrecondition,
        vec![OperationPrecondition::new(
            OperationPreconditionKind::TargetExists,
            OperationPreconditionStatus::Failed,
            Vec::new(),
            vec![path.as_str().to_owned()],
            PreconditionValue::Boolean { value: true },
            PreconditionValue::Boolean { value: false },
        )],
    )
}

fn already_exists_refusal(path: &CoreProjectPath) -> ChangeResult {
    ChangeResult::refused(
        RefusalReason::UnmetPrecondition,
        vec![OperationPrecondition::new(
            OperationPreconditionKind::TargetExists,
            OperationPreconditionStatus::Failed,
            Vec::new(),
            vec![path.as_str().to_owned()],
            PreconditionValue::Boolean { value: false },
            PreconditionValue::Boolean { value: true },
        )],
    )
}

fn source_drift_refusal(path: &CoreProjectPath, indexed: &str, disk: &str) -> ChangeResult {
    ChangeResult::refused(
        RefusalReason::UnmetPrecondition,
        vec![OperationPrecondition::new(
            OperationPreconditionKind::SourceUnchanged,
            OperationPreconditionStatus::Failed,
            Vec::new(),
            vec![path.as_str().to_owned()],
            PreconditionValue::Text {
                value: digest_hex8(indexed),
            },
            PreconditionValue::Text {
                value: digest_hex8(disk),
            },
        )],
    )
}

fn deletion_incomplete_refusal(path: &CoreProjectPath, remaining: &str) -> ChangeResult {
    let lines = remaining.split_inclusive('\n').count();
    ChangeResult::refused(
        RefusalReason::UnmetPrecondition,
        vec![OperationPrecondition::new(
            OperationPreconditionKind::SourceUnchanged,
            OperationPreconditionStatus::Failed,
            Vec::new(),
            vec![path.as_str().to_owned()],
            PreconditionValue::Text {
                value: "file fully deleted".to_owned(),
            },
            PreconditionValue::Text {
                value: truncate_detail(format!("{lines} line(s) remain after applying the hunks")),
            },
        )],
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
        ChangeResult, ChangeSummary, OperationPreconditionKind, PatchParams, PreconditionValue,
        RefusalReason,
    };
    use rift_protocol::read::ProjectPath;

    use super::{
        FileRewrite, PATCH_MISMATCH_DETAIL_BYTES_MAX, PatchTarget, RewriteKind, apply_segment,
        find_hunk_position, resolve_patch_target, truncate_detail,
    };
    use crate::change::ChangeService;
    use crate::read::ReadService;

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
        }
    }

    #[test]
    fn apply_segment_matches_the_header_position_exactly() -> TestResult {
        let diff = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-one\n+ONE\n two\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let result = apply_segment("one\ntwo\n", &parsed)
            .map_err(|_| "must apply at the exact header position")?;
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
            .map_err(|_| "context three lines below the header must still be found")?;
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
            .map_err(|_| "context three lines above the header must still be found")?;
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
            .map_err(|_| "the second hunk must land after correcting for the first hunk's drift")?;
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
        assert_eq!(detail.expected, "vanished");
        assert_eq!(detail.observed, "one");
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
        // now-patched line 1 and the untouched line 3 — the backward
        // position is tried first per the tie-break rule, but it must be
        // rejected for being already patched, landing on line 3 instead.
        let diff = "--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+MARK\n@@ -2 +2 @@\n-a\n+ZULU\n";
        let segments = hunks(diff)?;
        let parsed = Patch::from_str(&segments[0])?;
        let result = apply_segment("a\nz\na\n", &parsed)
            .map_err(|_| "the untouched occurrence must still be found")?;
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
        let content = apply_segment("", &parsed).map_err(|_| "an all-addition hunk must apply")?;
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
        let content = apply_segment("", &parsed).map_err(|_| "zero hunks must trivially apply")?;
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
        assert_ne!(RewriteKind::Modify, RewriteKind::Create);
        assert_ne!(RewriteKind::Create, RewriteKind::Delete);
        let _ = FileRewrite {
            path: rift_core::ProjectPath::new("f.rs")?,
            kind: RewriteKind::Delete,
            previous_len: 0,
            next_source: String::new(),
        };
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
        let result = changes.patch(&reads, &PatchParams { patch })?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths, vec![ProjectPath("new.rs".to_owned())]);
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
        let result = changes.patch(&reads, &PatchParams { patch })?;
        applied_summary(result);
        let written = fs::read_to_string(directory.path().join("a/b/c/new.rs"))?;
        assert_eq!(written, "pub fn nested() {}\n");
        Ok(())
    }

    #[test]
    fn patch_creates_an_empty_file() -> TestResult {
        let (directory, reads, changes) = fixture("pub fn beacon() {}\n")?;
        let patch = "--- /dev/null\n+++ b/empty.rs\n".to_owned();
        let result = changes.patch(&reads, &PatchParams { patch })?;
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
                    patch: "--- /dev/null\n+++ b//etc/passwd\n@@ -0,0 +1 @@\n+x\n".to_owned(),
                },
            )
            .expect_err("an absolute creation path must error");
        assert_eq!(absolute.descriptor().code(), "invalid_request");
        let dotted = changes
            .patch(
                &reads,
                &PatchParams {
                    patch: "--- /dev/null\n+++ b/../escape.rs\n@@ -0,0 +1 @@\n+x\n".to_owned(),
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
        let result = changes.patch(&reads, &PatchParams { patch })?;
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
            .patch(&reads, &PatchParams { patch })
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
        let result = changes.patch(&reads, &PatchParams { patch })?;
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
        let result = changes.patch(&reads, &PatchParams { patch })?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths, vec![ProjectPath("lib.rs".to_owned())]);
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
        let result = changes.patch(&reads, &PatchParams { patch })?;
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
        let result = changes.patch(&reads, &PatchParams { patch })?;
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
        let result = changes.patch(&reads, &PatchParams { patch })?;
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
        let result = changes.patch(&reads, &PatchParams { patch })?;
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
        let result = changes.patch(&reads, &PatchParams { patch })?;
        let summary = applied_summary(result);
        assert_eq!(summary.paths, vec![ProjectPath("lib.rs".to_owned())]);
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
        let result = changes.patch(&reads, &PatchParams { patch })?;
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
}
