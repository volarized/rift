//! Where a write is allowed to land, and how its bytes reach disk.
//!
//! Every write path in this crate resolves its target through
//! [`resolve_write_target`] before a byte is staged, and every whole-file
//! write publishes through [`publish_rewrites`]. Centralizing both here
//! means the workspace's `[source]` visibility policy and symlink
//! resolution are asked once, by the operation that actually touches the
//! filesystem, rather than by each tool that composes a rewrite.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use rift_core::ProjectPath as CoreProjectPath;
use rift_protocol::change::ChangeResult;
use rift_protocol::read::{
    Diagnostic, DiagnosticContinuation, DiagnosticReliability, Extensions, Severity,
};

use crate::read::{ReadError, ReadFault, ReadService};
use crate::rewrite::{FileRewrite, REWRITE_FILE_BYTES_MAX, RewriteKind, RewritePermissions};

/// Whether a write should publish through a symlink's resolved target, or
/// act on the addressed entry itself.
///
/// Every write kind but a deletion publishes beside the resolved target,
/// so the link itself is never replaced. A deletion removes exactly what
/// the caller addressed - the link, when the target is one - and never
/// follows it: following it would remove the link's target instead and
/// leave the link dangling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymlinkResolution {
    /// Publish beside the resolved target.
    Resolve,
    /// Act on the addressed entry itself; a symlink is never followed.
    Addressed,
}

/// Where one write is allowed to land: the path the caller addressed, and
/// the absolute filesystem location its bytes actually publish to.
///
/// For a requested path that is a symlink resolved under
/// [`SymlinkResolution::Resolve`], publishing lands beside the resolved
/// target instead of replacing the link, and `resolved` names that
/// target's own project path.
#[derive(Debug)]
pub(crate) struct WriteTarget {
    /// The path the caller addressed.
    path: CoreProjectPath,
    /// The absolute location this write's bytes land at.
    absolute: PathBuf,
    /// The resolved target's project path, set only when `path` is a
    /// symlink this write published through.
    resolved: Option<CoreProjectPath>,
}

impl WriteTarget {
    /// A warning naming both the requested link and the target its
    /// publish landed on, carried on a change result that wrote through a
    /// symlink. `None` when this target is not a symlink.
    fn symlink_warning(&self) -> Option<Diagnostic> {
        let resolved = self.resolved.as_ref()?;
        Some(symlink_diagnostic(&self.path, resolved))
    }
}

/// One finding naming both paths of a write that resolved through a
/// symlink: the requested link, and the target its bytes actually landed
/// at.
fn symlink_diagnostic(requested: &CoreProjectPath, resolved: &CoreProjectPath) -> Diagnostic {
    Diagnostic {
        severity: Severity::Warning,
        code: None,
        message: format!(
            "{} is a symlink; the write published to its resolved target {}",
            requested.as_str(),
            resolved.as_str()
        ),
        span: None,
        related: Vec::new(),
        tags: Vec::new(),
        reliability: DiagnosticReliability::Reliable,
        continuation: DiagnosticContinuation::Unknown,
        extensions: Extensions(BTreeMap::new()),
        language: None,
    }
}

/// Refuses a target the `[source]` policy makes invisible: excluded by
/// `[source].exclude`, `.gitignore`, or the hard floor. The file is
/// there; the workspace asked for it not to be.
pub(crate) fn not_visible_refusal(path: &CoreProjectPath) -> ChangeResult {
    crate::rename::unsupported_refusal(format!(
        "{} is outside the workspace's [source] visibility policy",
        path.as_str()
    ))
}

/// Refuses a symlinked write target whose resolved target the `[source]`
/// policy makes invisible, naming both the link and its target.
fn symlink_target_not_visible_refusal(
    requested: &CoreProjectPath,
    resolved: &CoreProjectPath,
) -> ChangeResult {
    crate::rename::unsupported_refusal(format!(
        "{} is a symlink whose target {} is outside the workspace's [source] \
         visibility policy",
        requested.as_str(),
        resolved.as_str()
    ))
}

/// Refuses a symlinked write target that cannot publish: its resolved
/// target lies outside the workspace, or the link itself cannot be
/// resolved (broken, or a cycle). `detail` names what resolution found.
fn symlink_unresolved_refusal(requested: &CoreProjectPath, detail: &str) -> ChangeResult {
    crate::rename::unsupported_refusal(format!(
        "{} is a symlink that cannot be published: {detail}",
        requested.as_str()
    ))
}

/// Whether `absolute` is visible under the workspace's `[source]` policy.
/// A snapshot with no policy - a revision snapshot, which has no
/// filesystem tree to be visible in - makes everything invisible.
fn visible(reads: &ReadService, absolute: &Path) -> bool {
    reads
        .source_policy()
        .is_some_and(|policy| policy.visible(absolute))
}

/// Resolves whether `path` is a legal write target: visible under the
/// workspace's `[source]` policy, and, when it is a symlink and
/// `resolution` is [`SymlinkResolution::Resolve`], resolved to a target
/// that stays inside the workspace and is visible in its own right. Every
/// write path in this crate calls this before a byte is staged; after
/// this call, staging and publishing use the returned target's absolute
/// location rather than rejoining `root` themselves.
///
/// # Errors
///
/// Returns [`ReadError`] for a filesystem failure other than the target's
/// own absence.
pub(crate) fn resolve_write_target(
    reads: &ReadService,
    root: &Path,
    path: &CoreProjectPath,
    resolution: SymlinkResolution,
) -> Result<Result<WriteTarget, ChangeResult>, ReadError> {
    let absolute = root.join(path.as_str());
    if !visible(reads, &absolute) {
        return Ok(Err(not_visible_refusal(path)));
    }
    match fs::symlink_metadata(&absolute) {
        Ok(metadata) if metadata.is_symlink() && resolution == SymlinkResolution::Resolve => {
            resolved_symlink_target(reads, root, path, &absolute)
        }
        Ok(_) => Ok(Ok(plain_target(path, absolute))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(Ok(plain_target(path, absolute)))
        }
        Err(error) => Err(ReadFault::storage(path.as_str(), "stat", &error)),
    }
}

/// A write target that is not a symlink: its bytes land where the
/// requested path itself joins the workspace root.
fn plain_target(path: &CoreProjectPath, absolute: PathBuf) -> WriteTarget {
    WriteTarget {
        path: path.clone(),
        absolute,
        resolved: None,
    }
}

/// Resolves a symlinked write target: canonicalizes the link, requires
/// the result to stay inside the workspace and pass the same visibility
/// check as the requested path, and publishes beside it so the link
/// itself is never replaced.
fn resolved_symlink_target(
    reads: &ReadService,
    root: &Path,
    path: &CoreProjectPath,
    absolute: &Path,
) -> Result<Result<WriteTarget, ChangeResult>, ReadError> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| ReadFault::storage(path.as_str(), "stat", &error))?;
    let resolved = match fs::canonicalize(absolute) {
        Ok(resolved) => resolved,
        Err(error) => {
            let raw_target = fs::read_link(absolute).map_or_else(
                |_| "an unreadable target".to_owned(),
                |target| target.display().to_string(),
            );
            return Ok(Err(symlink_unresolved_refusal(
                path,
                &format!("its target {raw_target} could not be resolved: {error}"),
            )));
        }
    };
    let Ok(stripped) = resolved.strip_prefix(&canonical_root) else {
        return Ok(Err(symlink_unresolved_refusal(
            path,
            &format!(
                "its resolved target {} is outside the workspace",
                resolved.display()
            ),
        )));
    };
    let Some(relative) = relative_project_path(stripped) else {
        return Ok(Err(symlink_unresolved_refusal(
            path,
            &format!(
                "its resolved target {} is not a legal project path",
                resolved.display()
            ),
        )));
    };
    if !visible(reads, &resolved) {
        return Ok(Err(symlink_target_not_visible_refusal(path, &relative)));
    }
    Ok(Ok(WriteTarget {
        path: path.clone(),
        absolute: resolved,
        resolved: Some(relative),
    }))
}

/// The project-relative path `relative` names, its segments joined with
/// `/` regardless of the operating system's own separator.
fn relative_project_path(relative: &Path) -> Option<CoreProjectPath> {
    let mut joined = String::new();
    for component in relative.components() {
        let text = component.as_os_str().to_str()?;
        if !joined.is_empty() {
            joined.push('/');
        }
        joined.push_str(text);
    }
    CoreProjectPath::new(joined).ok()
}

/// One target's bytes and permissions before batch publish, or absence.
enum PreviousState {
    Absent,
    Present {
        bytes: Vec<u8>,
        permissions: fs::Permissions,
    },
}

/// Reads target bytes and permissions for rollback.
fn captured_previous_state(path: &str, absolute: &Path) -> Result<PreviousState, ReadError> {
    let mut file = match fs::File::open(absolute) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PreviousState::Absent);
        }
        Err(error) => return Err(ReadFault::storage(path, "read", &error)),
    };
    let permissions = file
        .metadata()
        .map_err(|error| ReadFault::storage(path, "metadata", &error))?
        .permissions();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| ReadFault::storage(path, "read", &error))?;
    Ok(PreviousState::Present { bytes, permissions })
}

/// Opens one source and captures permissions from that same file handle.
fn captured_permissions(path: &str, absolute: &Path) -> Result<fs::Permissions, ReadError> {
    let file =
        fs::File::open(absolute).map_err(|error| ReadFault::storage(path, "read", &error))?;
    file.metadata()
        .map(|metadata| metadata.permissions())
        .map_err(|error| ReadFault::storage(path, "metadata", &error))
}

/// Directories staging created while making room for a `Create` rewrite,
/// recorded across the whole batch so rollback can remove them once
/// emptied, regardless of which member created which segment of a shared
/// path.
#[derive(Default)]
struct CreatedDirectories(Vec<PathBuf>);

impl CreatedDirectories {
    /// Creates `parent` and any missing ancestor up to `root`.
    ///
    /// Every missing ancestor is recorded before creation is attempted:
    /// `create_dir_all` is not atomic, so a failure partway through the
    /// chain can still leave some of these directories on disk, and only
    /// a directory recorded here is ever a candidate for cleanup.
    fn record(&mut self, root: &Path, parent: &Path) -> std::io::Result<()> {
        let mut missing = Vec::new();
        let mut candidate = parent;
        while candidate != root && !candidate.exists() {
            missing.push(candidate.to_path_buf());
            match candidate.parent() {
                Some(next) => candidate = next,
                None => break,
            }
        }
        self.0.extend(missing);
        fs::create_dir_all(parent)?;
        Ok(())
    }

    /// Removes every recorded directory that is now empty, deepest first,
    /// so a directory nested inside another the batch also created can
    /// clear its parent in the same pass. Best-effort: a directory still
    /// holding something - another member's surviving file, or content
    /// that predates this batch - is left in place.
    fn remove_if_empty(&self) {
        let mut directories = self.0.clone();
        directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        directories.dedup();
        for directory in directories {
            let _ = fs::remove_dir(&directory);
        }
    }
}

/// One rewrite staged for this batch's publish: the absolute location it
/// publishes to, its captured previous state, the warning to carry when
/// it resolved through a symlink, and its staged tempfile, present for
/// every kind but a deletion.
struct StagedMember<'batch> {
    rewrite: &'batch FileRewrite,
    absolute: PathBuf,
    previous: PreviousState,
    warning: Option<Diagnostic>,
    staged: Option<tempfile::NamedTempFile>,
    /// Permissions applied after publication, because Windows persistence clears attributes.
    permissions: Option<fs::Permissions>,
}

/// Captures previous state and stages next content in target directory.
fn stage_member<'batch>(
    root: &Path,
    rewrite: &'batch FileRewrite,
    target: &WriteTarget,
    created: &mut CreatedDirectories,
) -> Result<StagedMember<'batch>, ReadError> {
    let previous = if matches!(rewrite.kind, RewriteKind::Create) {
        PreviousState::Absent
    } else {
        captured_previous_state(rewrite.path.as_str(), &target.absolute)?
    };
    let permissions = match (&previous, &rewrite.permissions) {
        (_, RewritePermissions::Exact(permissions)) => Some(permissions.clone()),
        (_, RewritePermissions::RetainFrom(source)) => Some(captured_permissions(
            source.as_str(),
            &root.join(source.as_str()),
        )?),
        (PreviousState::Present { permissions, .. }, RewritePermissions::Retain) => {
            Some(permissions.clone())
        }
        (PreviousState::Absent, RewritePermissions::Retain) => None,
    };
    let warning = target.symlink_warning();
    if rewrite.kind.removes_file() {
        return Ok(StagedMember {
            rewrite,
            absolute: target.absolute.clone(),
            previous,
            warning,
            staged: None,
            permissions: None,
        });
    }
    let parent = target.absolute.parent().unwrap_or(root);
    if matches!(rewrite.kind, RewriteKind::Create) {
        created
            .record(root, parent)
            .map_err(|error| ReadFault::storage(rewrite.path.as_str(), "create_dir", &error))?;
    }
    let mut file = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| ReadFault::storage(rewrite.path.as_str(), "stage", &error))?;
    file.write_all(rewrite.next_source.as_bytes())
        .map_err(|error| ReadFault::storage(rewrite.path.as_str(), "stage", &error))?;
    if let Some(permissions) = &permissions {
        file.as_file()
            .set_permissions(permissions.clone())
            .map_err(|error| ReadFault::storage(rewrite.path.as_str(), "permissions", &error))?;
    }
    Ok(StagedMember {
        rewrite,
        absolute: target.absolute.clone(),
        previous,
        warning,
        staged: Some(file),
        permissions,
    })
}

/// Clears the Windows read-only attribute before replacing or deleting an existing file.
#[cfg(windows)]
fn make_target_replaceable(path: &Path) -> std::io::Result<bool> {
    let mut permissions = match fs::metadata(path) {
        Ok(metadata) => metadata.permissions(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if permissions.readonly() {
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
        return Ok(true);
    }
    Ok(false)
}

/// Applies captured permissions after persistence, including Windows attributes cleared by tempfile.
fn apply_published_permissions(
    path: &Path,
    permissions: Option<&fs::Permissions>,
) -> std::io::Result<()> {
    permissions.map_or(Ok(()), |permissions| {
        fs::set_permissions(path, permissions.clone())
    })
}

/// Publishes one staged member: a deletion removes its file; a creation
/// persists its staged tempfile only if nothing has come to stand at the
/// target since it was resolved, and refuses instead of clobbering it
/// otherwise; every other kind persists its staged tempfile onto the
/// target unconditionally, through a rename within one directory.
fn publish_member(member: &mut StagedMember<'_>, mutated: &mut bool) -> Result<(), ReadError> {
    #[cfg(windows)]
    if !matches!(member.rewrite.kind, RewriteKind::Create) {
        *mutated = make_target_replaceable(&member.absolute).map_err(|error| {
            ReadFault::storage(member.rewrite.path.as_str(), "permissions", &error)
        })?;
    }
    let outcome: Result<(), std::io::Error> = match (&member.rewrite.kind, member.staged.take()) {
        (RewriteKind::Delete, _) => fs::remove_file(&member.absolute),
        (RewriteKind::Create, Some(staged)) => staged
            .persist_noclobber(&member.absolute)
            .map(drop)
            .map_err(|error| error.error),
        (_, Some(staged)) => staged
            .persist(&member.absolute)
            .map(drop)
            .map_err(|error| error.error),
        (_, None) => Err(std::io::Error::other(
            "staged rewrite is missing its staged file",
        )),
    };
    let outcome = outcome.and_then(|()| {
        *mutated = true;
        if member.rewrite.kind.removes_file() {
            Ok(())
        } else {
            apply_published_permissions(&member.absolute, member.permissions.as_ref())
        }
    });
    outcome.map_err(|error| ReadFault::storage(member.rewrite.path.as_str(), "publish", &error))
}

/// Publishes every staged member in order; on the first failure, restores
/// every member already published and returns the failure that stopped
/// the publish.
fn publish_staged(staged: &mut [StagedMember<'_>]) -> Result<(), ReadError> {
    for index in 0..staged.len() {
        let mut mutated = false;
        if let Err(error) = publish_member(&mut staged[index], &mut mutated) {
            let published = if mutated {
                &staged[..=index]
            } else {
                &staged[..index]
            };
            roll_back_published(published);
            return Err(error);
        }
    }
    Ok(())
}

/// Restores every published member to captured bytes and permissions.
/// Restoration remains best-effort because caller returns original publish error.
fn roll_back_published(published: &[StagedMember<'_>]) {
    for member in published {
        match &member.previous {
            PreviousState::Present { bytes, permissions } => {
                let parent = member.absolute.parent().unwrap_or_else(|| Path::new("."));
                #[cfg(windows)]
                let _ = make_target_replaceable(&member.absolute);
                if let Ok(mut restored) = tempfile::NamedTempFile::new_in(parent)
                    && restored.write_all(bytes).is_ok()
                    && restored
                        .as_file()
                        .set_permissions(permissions.clone())
                        .is_ok()
                    && restored.persist(&member.absolute).is_ok()
                {
                    let _ = fs::set_permissions(&member.absolute, permissions.clone());
                }
            }
            PreviousState::Absent => {
                #[cfg(windows)]
                let _ = make_target_replaceable(&member.absolute);
                let _ = fs::remove_file(&member.absolute);
            }
        }
    }
}

/// Discards every staged member: a not-yet-published member's tempfile
/// is dropped, which removes it, and any directory this batch created for
/// it is removed while it is still empty. Consuming `staged` by value
/// forces every live tempfile to drop before `created` is asked whether
/// its directories are now empty.
fn discard_unpublished(staged: Vec<StagedMember<'_>>, created: &CreatedDirectories) {
    drop(staged);
    created.remove_if_empty();
}

/// `absolute`, canonicalized when the filesystem can resolve it. A
/// workspace root can itself sit behind a symlink (macOS's `/tmp` is
/// one), so a symlink target and its non-symlink alias can join the same
/// `root` into two different address strings for one real file; comparing
/// the canonical form is what makes them equal. A `Create` target has
/// nothing to canonicalize yet, so its own address stands in unchanged.
fn canonical_or_addressed(absolute: &Path) -> PathBuf {
    fs::canonicalize(absolute).unwrap_or_else(|_| absolute.to_path_buf())
}

/// Refuses a batch whose members publish to the same absolute location: a
/// symlink and its resolved target, addressed as two separate members,
/// resolve to that same location just as two members naming one path
/// literally would - the second stages over the first, and neither
/// member's bytes survive the publish.
fn duplicate_target_refusal(
    rewrites: &[FileRewrite],
    targets: &[WriteTarget],
) -> Option<ChangeResult> {
    let mut seen: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    rewrites
        .iter()
        .zip(targets)
        .find(|(_, target)| !seen.insert(canonical_or_addressed(&target.absolute)))
        .map(|(rewrite, _)| {
            crate::rename::unsupported_refusal(format!(
                "{} is addressed by more than one member of this batch",
                rewrite.path.as_str()
            ))
        })
}

/// Refuses a batch containing one rewrite whose resulting file exceeds
/// [`REWRITE_FILE_BYTES_MAX`], naming the file and its byte count. Checked before any
/// member stages, so this bounds every write path that produces a [`FileRewrite`]: a
/// create edit, the whole-file fallback a batch reports past the change result's own
/// edit bound, and a small patch against a file already over the bound.
fn oversized_rewrite_refusal(rewrites: &[FileRewrite]) -> Option<ChangeResult> {
    rewrites
        .iter()
        .find(|rewrite| rewrite.next_source.len() > REWRITE_FILE_BYTES_MAX)
        .map(|rewrite| {
            crate::rename::unsupported_refusal(format!(
                "{} would hold {} bytes; at most {REWRITE_FILE_BYTES_MAX} are rewritten",
                rewrite.path.as_str(),
                rewrite.next_source.len()
            ))
        })
}

/// Stages and publishes whole-file rewrites, all or none.
///
/// Every rewrite resolves through [`resolve_write_target`] before
/// anything stages, a deletion addressed at the entry itself and every
/// other kind resolved through a symlink it names; one invisible or
/// symlink-refused member refuses the whole batch and stages nothing, as
/// does a batch whose members resolve to the same absolute target - a
/// symlink and its resolved target named separately included - or one
/// holding a resulting file past [`REWRITE_FILE_BYTES_MAX`]. Staging
/// uses an exclusive temporary file per target, so no workspace filename
/// can collide with it, and every target's previous state is captured
/// before the first publish. A failed publish restores every member
/// already published from that capture and removes any directory this
/// batch created and left empty.
///
/// Returns the warnings a symlink-resolved member carries; the caller
/// attaches them to its own result alongside whatever else it reports.
///
/// # Errors
///
/// Returns [`ReadError`] for a filesystem failure; an invisible target, a
/// duplicated batch member, or an oversized resulting file returns a
/// refused [`ChangeResult`] instead.
pub(crate) fn publish_rewrites(
    reads: &ReadService,
    root: &Path,
    rewrites: &[FileRewrite],
) -> Result<Result<Vec<Diagnostic>, ChangeResult>, ReadError> {
    if let Some(refusal) = oversized_rewrite_refusal(rewrites) {
        return Ok(Err(refusal));
    }
    let mut targets: Vec<WriteTarget> = Vec::with_capacity(rewrites.len());
    for rewrite in rewrites {
        let resolution = if rewrite.kind.removes_file() {
            SymlinkResolution::Addressed
        } else {
            SymlinkResolution::Resolve
        };
        match resolve_write_target(reads, root, &rewrite.path, resolution)? {
            Ok(target) => targets.push(target),
            Err(refusal) => return Ok(Err(refusal)),
        }
    }
    if let Some(refusal) = duplicate_target_refusal(rewrites, &targets) {
        return Ok(Err(refusal));
    }
    let mut created_directories = CreatedDirectories::default();
    let mut staged: Vec<StagedMember<'_>> = Vec::with_capacity(rewrites.len());
    for (rewrite, target) in rewrites.iter().zip(&targets) {
        match stage_member(root, rewrite, target, &mut created_directories) {
            Ok(member) => staged.push(member),
            Err(error) => {
                discard_unpublished(staged, &created_directories);
                return Err(error);
            }
        }
    }
    if let Err(error) = publish_staged(&mut staged) {
        discard_unpublished(staged, &created_directories);
        return Err(error);
    }
    Ok(Ok(staged
        .iter()
        .filter_map(|member| member.warning.clone())
        .collect()))
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use rift_core::{ProjectPath as CoreProjectPath, SourceVisibility, TextFileInclusion};
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::change::{ChangeResult, RefusalReason};
    use rift_protocol::configuration::HistoryConfiguration;
    use rift_syntax::ByteRange;

    use super::{
        CreatedDirectories, PreviousState, StagedMember, SymlinkResolution, publish_member,
        publish_rewrites, resolve_write_target, resolved_symlink_target,
    };
    use crate::read::ReadService;
    use crate::rewrite::{FileRewrite, REWRITE_FILE_BYTES_MAX, ReplacedRegion};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn path(value: &str) -> CoreProjectPath {
        CoreProjectPath::new(value).expect("fixture path is valid")
    }

    fn reads_over(
        root: &std::path::Path,
        visibility: &SourceVisibility,
    ) -> TestResult<ReadService> {
        Ok(ReadService::build(
            root,
            WorkspaceIndexLimits::default(),
            visibility,
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?)
    }

    #[test]
    fn test_resolve_write_target_refuses_an_excluded_path() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("excluded"))?;
        fs::write(
            directory.path().join("excluded/hidden.rs"),
            "pub fn h() {}\n",
        )?;
        let visibility = SourceVisibility::new(Vec::new(), vec!["excluded/**".to_owned()], true);
        let reads = reads_over(directory.path(), &visibility)?;
        let refusal = resolve_write_target(
            &reads,
            directory.path(),
            &path("excluded/hidden.rs"),
            SymlinkResolution::Resolve,
        )?
        .expect_err("an excluded path must refuse");
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = refusal
        else {
            panic!("an excluded path must refuse");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert!(
            diagnostics[0].message.contains("excluded/hidden.rs")
                && diagnostics[0].message.contains("[source]"),
            "{diagnostics:?}"
        );
        Ok(())
    }

    #[test]
    fn test_resolve_write_target_allows_a_visible_path() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let target = resolve_write_target(
            &reads,
            directory.path(),
            &path("lib.rs"),
            SymlinkResolution::Resolve,
        )?
        .expect("a visible path must resolve");
        assert_eq!(target.absolute, directory.path().join("lib.rs"));
        assert!(target.resolved.is_none());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_write_target_resolves_an_in_workspace_symlink() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("real.rs"), "pub fn real() {}\n")?;
        std::os::unix::fs::symlink("real.rs", directory.path().join("link.rs"))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let target = resolve_write_target(
            &reads,
            directory.path(),
            &path("link.rs"),
            SymlinkResolution::Resolve,
        )?
        .expect("an in-workspace symlink must resolve");
        assert_eq!(
            target.absolute,
            directory.path().canonicalize()?.join("real.rs")
        );
        assert_eq!(
            target.resolved.as_ref().map(CoreProjectPath::as_str),
            Some("real.rs")
        );
        assert!(target.symlink_warning().is_some());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_write_target_refuses_a_symlink_outside_the_workspace() -> TestResult {
        let directory = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        fs::write(outside.path().join("secret.rs"), "pub fn secret() {}\n")?;
        std::os::unix::fs::symlink(
            outside.path().join("secret.rs"),
            directory.path().join("link.rs"),
        )?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let refusal = resolve_write_target(
            &reads,
            directory.path(),
            &path("link.rs"),
            SymlinkResolution::Resolve,
        )?
        .expect_err("a symlink resolving outside the workspace must refuse");
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = refusal
        else {
            panic!("an outside-workspace symlink must refuse");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert!(
            diagnostics[0].message.contains("link.rs")
                && diagnostics[0].message.contains("outside"),
            "{diagnostics:?}"
        );
        assert!(outside.path().join("secret.rs").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_write_target_refuses_a_broken_symlink() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::os::unix::fs::symlink("missing.rs", directory.path().join("link.rs"))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let refusal = resolve_write_target(
            &reads,
            directory.path(),
            &path("link.rs"),
            SymlinkResolution::Resolve,
        )?
        .expect_err("a broken symlink must refuse");
        let ChangeResult::Refused { reason, .. } = refusal else {
            panic!("a broken symlink must refuse");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        Ok(())
    }

    #[test]
    fn test_publish_rewrites_refuses_a_batch_naming_one_path_twice() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let rewrites = vec![
            FileRewrite::modify(
                path("lib.rs"),
                "pub fn beacon() {}\n",
                "pub fn one() {}\n".to_owned(),
                vec![ReplacedRegion {
                    range: ByteRange { start: 0, end: 20 },
                    text: "pub fn one() {}\n".to_owned(),
                }],
            ),
            FileRewrite::modify(
                path("lib.rs"),
                "pub fn beacon() {}\n",
                "pub fn two() {}\n".to_owned(),
                vec![ReplacedRegion {
                    range: ByteRange { start: 0, end: 20 },
                    text: "pub fn two() {}\n".to_owned(),
                }],
            ),
        ];
        let refusal = publish_rewrites(&reads, directory.path(), &rewrites)?
            .expect_err("a batch naming one path twice must refuse");
        assert!(matches!(
            refusal,
            ChangeResult::Refused {
                reason: RefusalReason::Unsupported,
                ..
            }
        ));
        assert_eq!(
            fs::read_to_string(directory.path().join("lib.rs"))?,
            "pub fn beacon() {}\n",
            "a refused batch writes nothing"
        );
        Ok(())
    }

    /// One member past [`REWRITE_FILE_BYTES_MAX`] refuses the whole batch atomically,
    /// including a sibling member that is well within the bound: a create edit, the
    /// whole-file fallback a change result reports past its own edit bound, and a
    /// small patch against an already oversized file all share this check.
    #[test]
    fn test_publish_rewrites_refuses_a_batch_with_one_oversized_result() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("small.rs"), "pub fn beacon() {}\n")?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let oversized_source = "x".repeat(REWRITE_FILE_BYTES_MAX + 1);
        let rewrites = vec![
            FileRewrite::modify(
                path("small.rs"),
                "pub fn beacon() {}\n",
                "pub fn beacon() -> u8 { 1 }\n".to_owned(),
                vec![ReplacedRegion {
                    range: ByteRange { start: 0, end: 20 },
                    text: "pub fn beacon() -> u8 { 1 }\n".to_owned(),
                }],
            ),
            FileRewrite::create(path("huge.rs"), oversized_source),
        ];
        let refusal = publish_rewrites(&reads, directory.path(), &rewrites)?
            .expect_err("an oversized member must refuse the whole batch");
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = refusal
        else {
            panic!("an oversized result must refuse");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert!(
            diagnostics[0].message.contains("huge.rs")
                && diagnostics[0]
                    .message
                    .contains(&(REWRITE_FILE_BYTES_MAX + 1).to_string()),
            "{diagnostics:?}"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("small.rs"))?,
            "pub fn beacon() {}\n",
            "a refused batch writes nothing, including its well-bounded member"
        );
        assert!(
            !directory.path().join("huge.rs").exists(),
            "the oversized member's file must not be created"
        );
        Ok(())
    }

    /// A resulting file exactly at [`REWRITE_FILE_BYTES_MAX`] publishes.
    #[test]
    fn test_publish_rewrites_accepts_a_result_exactly_at_the_bound() -> TestResult {
        let directory = tempfile::tempdir()?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let at_bound = "x".repeat(REWRITE_FILE_BYTES_MAX);
        let rewrites = vec![FileRewrite::create(path("at_bound.rs"), at_bound.clone())];
        publish_rewrites(&reads, directory.path(), &rewrites)?
            .expect("a result exactly at the bound must publish");
        assert_eq!(
            fs::read_to_string(directory.path().join("at_bound.rs"))?,
            at_bound
        );
        Ok(())
    }

    /// A directory standing where a file is expected forces the second
    /// member's publish to fail; the first member, already published,
    /// must roll back from its captured bytes rather than from the
    /// syntax index, which never indexed the `.txt` file at all.
    #[test]
    fn test_publish_rewrites_restores_an_unindexed_member_after_a_forced_publish_failure()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("notes.txt"), "original notes\n")?;
        fs::create_dir(directory.path().join("blocked.rs"))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let rewrites = vec![
            FileRewrite::modify(
                path("notes.txt"),
                "original notes\n",
                "changed notes\n".to_owned(),
                vec![ReplacedRegion {
                    range: ByteRange { start: 0, end: 15 },
                    text: "changed notes\n".to_owned(),
                }],
            ),
            FileRewrite::create(path("blocked.rs"), "pub fn blocked() {}\n".to_owned()),
        ];
        let error = publish_rewrites(&reads, directory.path(), &rewrites)
            .expect_err("a directory standing where a file is expected must fail the publish");
        assert_eq!(error.descriptor().code(), "storage_failure");
        assert_eq!(
            fs::read_to_string(directory.path().join("notes.txt"))?,
            "original notes\n",
            "the unindexed member must roll back to its captured bytes"
        );
        assert!(
            directory.path().join("blocked.rs").is_dir(),
            "the blocking directory is untouched"
        );
        Ok(())
    }

    #[cfg(unix)]
    fn permission_mode(path: &std::path::Path) -> std::io::Result<u32> {
        Ok(fs::metadata(path)?.permissions().mode() & 0o777)
    }

    #[cfg(unix)]
    #[test]
    fn test_publish_rewrites_preserves_executable_permissions() -> TestResult {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("run.sh");
        let source = "#!/bin/sh\necho old\n";
        fs::write(&target, source)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o751))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let next = "#!/bin/sh\necho new\n";
        let rewrites = vec![FileRewrite::modify(
            path("run.sh"),
            source,
            next.to_owned(),
            vec![ReplacedRegion {
                range: ByteRange { start: 15, end: 18 },
                text: "new".to_owned(),
            }],
        )];
        publish_rewrites(&reads, directory.path(), &rewrites)?
            .expect("an executable rewrite must publish");
        assert_eq!(fs::read_to_string(&target)?, next);
        assert_eq!(
            permission_mode(&target)?,
            0o751,
            "publishing new bytes must retain executable permissions"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_publish_rewrites_preserves_non_executable_permissions() -> TestResult {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("notes.txt");
        let source = "old notes\n";
        fs::write(&target, source)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let next = "new notes\n";
        let rewrites = vec![FileRewrite::modify(
            path("notes.txt"),
            source,
            next.to_owned(),
            vec![ReplacedRegion {
                range: ByteRange { start: 0, end: 3 },
                text: "new".to_owned(),
            }],
        )];
        publish_rewrites(&reads, directory.path(), &rewrites)?
            .expect("a non-executable rewrite must publish");
        assert_eq!(fs::read_to_string(&target)?, next);
        assert_eq!(
            permission_mode(&target)?,
            0o640,
            "publishing new bytes must retain non-executable permissions"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_publish_rewrites_rollback_restores_permissions() -> TestResult {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("run.sh");
        let source = "#!/bin/sh\necho old\n";
        fs::write(&target, source)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o751))?;
        fs::create_dir(directory.path().join("blocked.rs"))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let rewrites = vec![
            FileRewrite::modify(
                path("run.sh"),
                source,
                "#!/bin/sh\necho new\n".to_owned(),
                vec![ReplacedRegion {
                    range: ByteRange { start: 15, end: 18 },
                    text: "new".to_owned(),
                }],
            ),
            FileRewrite::create(path("blocked.rs"), "pub fn blocked() {}\n".to_owned()),
        ];
        let error = publish_rewrites(&reads, directory.path(), &rewrites)
            .expect_err("a directory standing where a file is expected must fail the publish");
        assert_eq!(error.descriptor().code(), "storage_failure");
        assert_eq!(fs::read_to_string(&target)?, source);
        assert_eq!(
            permission_mode(&target)?,
            0o751,
            "rollback must restore captured permissions with captured bytes"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_publish_rewrites_rollback_restores_deleted_file_permissions() -> TestResult {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("run.sh");
        let source = "#!/bin/sh\necho old\n";
        fs::write(&target, source)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o751))?;
        fs::create_dir(directory.path().join("blocked.rs"))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let rewrites = vec![
            FileRewrite::delete(path("run.sh"), source),
            FileRewrite::create(path("blocked.rs"), "pub fn blocked() {}\n".to_owned()),
        ];
        let error = publish_rewrites(&reads, directory.path(), &rewrites)
            .expect_err("a directory standing where a file is expected must fail the publish");
        assert_eq!(error.descriptor().code(), "storage_failure");
        assert_eq!(fs::read_to_string(&target)?, source);
        assert_eq!(
            permission_mode(&target)?,
            0o751,
            "rollback must recreate a deleted file with captured permissions"
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn test_publish_rewrites_preserves_readonly_permissions() -> TestResult {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("notes.txt");
        let source = "old notes\n";
        fs::write(&target, source)?;
        let mut permissions = fs::metadata(&target)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&target, permissions)?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let next = "new notes\n";
        let rewrites = vec![FileRewrite::modify(
            path("notes.txt"),
            source,
            next.to_owned(),
            vec![ReplacedRegion {
                range: ByteRange { start: 0, end: 3 },
                text: "new".to_owned(),
            }],
        )];

        publish_rewrites(&reads, directory.path(), &rewrites)?
            .expect("a read-only rewrite must publish");
        assert_eq!(fs::read_to_string(&target)?, next);
        assert!(
            fs::metadata(&target)?.permissions().readonly(),
            "publishing new bytes must retain the read-only state"
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn test_publish_rewrites_rollback_restores_readonly_permissions() -> TestResult {
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("notes.txt");
        let source = "old notes\n";
        fs::write(&target, source)?;
        let mut permissions = fs::metadata(&target)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&target, permissions)?;
        fs::create_dir(directory.path().join("blocked.rs"))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let rewrites = vec![
            FileRewrite::modify(
                path("notes.txt"),
                source,
                "new notes\n".to_owned(),
                vec![ReplacedRegion {
                    range: ByteRange { start: 0, end: 3 },
                    text: "new".to_owned(),
                }],
            ),
            FileRewrite::create(path("blocked.rs"), "pub fn blocked() {}\n".to_owned()),
        ];

        publish_rewrites(&reads, directory.path(), &rewrites)
            .expect_err("a directory standing where a file is expected must fail the publish");
        assert_eq!(fs::read_to_string(&target)?, source);
        assert!(
            fs::metadata(&target)?.permissions().readonly(),
            "rollback must restore captured read-only state with captured bytes"
        );
        Ok(())
    }

    #[test]
    fn test_publish_rewrites_removes_an_empty_directory_it_created_on_rollback() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("blocked.rs"))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let rewrites = vec![
            FileRewrite::create(path("nested/deep/new.rs"), "pub fn fresh() {}\n".to_owned()),
            FileRewrite::create(path("blocked.rs"), "pub fn blocked() {}\n".to_owned()),
        ];
        publish_rewrites(&reads, directory.path(), &rewrites)
            .expect_err("the blocking directory must fail the publish");
        assert!(
            !directory.path().join("nested").exists(),
            "the directory this batch created for the rolled-back create must be removed"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_write_target_refuses_a_symlink_whose_target_is_excluded() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("excluded"))?;
        fs::write(
            directory.path().join("excluded/real.rs"),
            "pub fn real() {}\n",
        )?;
        std::os::unix::fs::symlink("excluded/real.rs", directory.path().join("link.rs"))?;
        let visibility = SourceVisibility::new(Vec::new(), vec!["excluded/**".to_owned()], true);
        let reads = reads_over(directory.path(), &visibility)?;
        let refusal = resolve_write_target(
            &reads,
            directory.path(),
            &path("link.rs"),
            SymlinkResolution::Resolve,
        )?
        .expect_err("a symlink resolving to an excluded target must refuse");
        let ChangeResult::Refused {
            reason,
            diagnostics,
            ..
        } = refusal
        else {
            panic!("a symlink to an excluded target must refuse");
        };
        assert_eq!(reason, RefusalReason::Unsupported);
        assert!(
            diagnostics[0].message.contains("link.rs")
                && diagnostics[0].message.contains("excluded/real.rs")
                && diagnostics[0].message.contains("[source]"),
            "{diagnostics:?}"
        );
        Ok(())
    }

    /// `resolved_symlink_target` is exercised directly: `resolve_write_target`
    /// only reaches it after confirming the path is a symlink, and cannot
    /// reproduce the link vanishing between that check and this one.
    #[cfg(unix)]
    #[test]
    fn test_resolved_symlink_target_reports_an_unreadable_target_when_the_link_is_gone()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let link_absolute = directory.path().join("link.rs");
        std::os::unix::fs::symlink("missing.rs", &link_absolute)?;
        fs::remove_file(&link_absolute)?;
        let refusal =
            resolved_symlink_target(&reads, directory.path(), &path("link.rs"), &link_absolute)?
                .expect_err("a link removed mid-resolution must refuse");
        let ChangeResult::Refused { diagnostics, .. } = refusal else {
            panic!("a vanished symlink must refuse");
        };
        assert!(
            diagnostics[0].message.contains("an unreadable target"),
            "{diagnostics:?}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_write_target_resolves_a_symlink_into_a_nested_directory() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("sub"))?;
        fs::write(directory.path().join("sub/real.rs"), "pub fn real() {}\n")?;
        std::os::unix::fs::symlink("sub/real.rs", directory.path().join("link.rs"))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let target = resolve_write_target(
            &reads,
            directory.path(),
            &path("link.rs"),
            SymlinkResolution::Resolve,
        )?
        .expect("a nested in-workspace symlink must resolve");
        assert_eq!(
            target.resolved.as_ref().map(CoreProjectPath::as_str),
            Some("sub/real.rs")
        );
        Ok(())
    }

    /// A modify whose target has already vanished from disk - a concurrent
    /// deletion, not the create path, which never reads previous state at
    /// all - captures `Absent`, so a rollback removes it rather than
    /// restoring content that never existed.
    #[test]
    fn test_publish_rewrites_rolls_back_a_modify_whose_target_was_absent_at_capture_time()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("blocked.rs"))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let rewrites = vec![
            FileRewrite::modify(
                path("phantom.rs"),
                "",
                "pub fn phantom() {}\n".to_owned(),
                vec![ReplacedRegion {
                    range: ByteRange { start: 0, end: 0 },
                    text: "pub fn phantom() {}\n".to_owned(),
                }],
            ),
            FileRewrite::create(path("blocked.rs"), "pub fn blocked() {}\n".to_owned()),
        ];
        let error = publish_rewrites(&reads, directory.path(), &rewrites)
            .expect_err("the blocking directory must fail the publish");
        assert_eq!(error.descriptor().code(), "storage_failure");
        assert!(
            !directory.path().join("phantom.rs").exists(),
            "a modify whose target was absent at capture time rolls back to absent"
        );
        Ok(())
    }

    #[test]
    fn test_publish_rewrites_fails_when_a_modify_target_is_a_directory() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("blocked.rs"))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let rewrites = vec![FileRewrite::modify(
            path("blocked.rs"),
            "",
            "pub fn blocked() {}\n".to_owned(),
            vec![ReplacedRegion {
                range: ByteRange { start: 0, end: 0 },
                text: "pub fn blocked() {}\n".to_owned(),
            }],
        )];
        let error = publish_rewrites(&reads, directory.path(), &rewrites)
            .expect_err("a modify target that is a directory must fail before staging");
        assert_eq!(error.descriptor().code(), "storage_failure");
        assert!(
            directory.path().join("blocked.rs").is_dir(),
            "the directory is untouched"
        );
        Ok(())
    }

    /// `publish_member` refuses a non-delete member with no staged file
    /// rather than treating it as a no-op; `stage_member` never produces
    /// this combination, so the member is constructed directly.
    #[test]
    fn test_publish_member_refuses_a_non_delete_member_missing_its_staged_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        let rewrite = FileRewrite::create(path("new.rs"), "pub fn fresh() {}\n".to_owned());
        let mut member = StagedMember {
            rewrite: &rewrite,
            absolute: directory.path().join("new.rs"),
            previous: PreviousState::Absent,
            warning: None,
            staged: None,
            permissions: None,
        };
        let error = publish_member(&mut member, &mut false)
            .expect_err("a non-delete member with no staged file must refuse");
        assert_eq!(error.descriptor().code(), "storage_failure");
        let context = error.context();
        assert_eq!(context[1].value(), "publish");
        assert!(context[2].value().contains("missing its staged file"));
        Ok(())
    }

    /// A file appears at a `Create` target after `resolve_create` already
    /// proved it absent - a concurrent writer, not this batch. The publish
    /// must refuse rather than clobber it with `persist`, and the earlier
    /// member in the same batch must still roll back from its own captured
    /// bytes, never touching the file this create collided with.
    #[test]
    fn test_publish_rewrites_refuses_a_create_whose_target_appeared_since_the_check() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("notes.txt"), "original notes\n")?;
        fs::write(directory.path().join("new.rs"), "interloper content\n")?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let rewrites = vec![
            FileRewrite::modify(
                path("notes.txt"),
                "original notes\n",
                "changed notes\n".to_owned(),
                vec![ReplacedRegion {
                    range: ByteRange { start: 0, end: 15 },
                    text: "changed notes\n".to_owned(),
                }],
            ),
            FileRewrite::create(path("new.rs"), "pub fn fresh() {}\n".to_owned()),
        ];
        let error = publish_rewrites(&reads, directory.path(), &rewrites)
            .expect_err("a create whose target appeared since the check must fail the publish");
        assert_eq!(error.descriptor().code(), "storage_failure");
        assert!(
            directory.path().join("notes.txt").exists(),
            "the already-published member must remain present"
        );
        assert!(
            directory.path().join("new.rs").exists(),
            "the colliding create target must remain present"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("notes.txt"))?,
            "original notes\n",
            "the already-published member must roll back"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("new.rs"))?,
            "interloper content\n",
            "a colliding create must not clobber the file that appeared, nor be removed on rollback"
        );
        Ok(())
    }

    /// A `Delete` targeting a symlink must remove the link itself, never
    /// the entry it points to: following the link would destroy content
    /// the request never named and leave the link dangling.
    #[cfg(unix)]
    #[test]
    fn test_publish_rewrites_deletes_a_symlink_without_touching_its_target() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("real.rs"), "pub fn real() {}\n")?;
        std::os::unix::fs::symlink("real.rs", directory.path().join("link.rs"))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let rewrites = vec![FileRewrite::delete(path("link.rs"), "pub fn real() {}\n")];
        publish_rewrites(&reads, directory.path(), &rewrites)?
            .expect("deleting a symlink must succeed");
        assert!(
            fs::symlink_metadata(directory.path().join("link.rs")).is_err(),
            "the link itself must be gone"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("real.rs"))?,
            "pub fn real() {}\n",
            "the link's target must be untouched"
        );
        Ok(())
    }

    /// A batch that addresses a symlink and its resolved target as two
    /// separate members would publish both to the same absolute location;
    /// the second stages over the first and the result would report both
    /// as applied. The dedupe must catch this even though the two members
    /// name different paths.
    #[cfg(unix)]
    #[test]
    fn test_publish_rewrites_refuses_a_batch_naming_a_symlink_and_its_resolved_target() -> TestResult
    {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("real.rs"), "pub fn real() {}\n")?;
        std::os::unix::fs::symlink("real.rs", directory.path().join("link.rs"))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let rewrites = vec![
            FileRewrite::modify(
                path("link.rs"),
                "pub fn real() {}\n",
                "pub fn one() {}\n".to_owned(),
                vec![ReplacedRegion {
                    range: ByteRange { start: 0, end: 17 },
                    text: "pub fn one() {}\n".to_owned(),
                }],
            ),
            FileRewrite::modify(
                path("real.rs"),
                "pub fn real() {}\n",
                "pub fn two() {}\n".to_owned(),
                vec![ReplacedRegion {
                    range: ByteRange { start: 0, end: 17 },
                    text: "pub fn two() {}\n".to_owned(),
                }],
            ),
        ];
        let refusal = publish_rewrites(&reads, directory.path(), &rewrites)?
            .expect_err("a batch naming a symlink and its resolved target must refuse");
        assert!(matches!(
            refusal,
            ChangeResult::Refused {
                reason: RefusalReason::Unsupported,
                ..
            }
        ));
        assert_eq!(
            fs::read_to_string(directory.path().join("real.rs"))?,
            "pub fn real() {}\n",
            "a refused batch writes nothing"
        );
        Ok(())
    }

    /// The resolved target of `link.rs` lands inside the workspace's own
    /// `.rift/` state directory, so `strip_prefix` against the canonical
    /// root succeeds; `relative_project_path` then refuses it for an
    /// unrelated reason (it names Rift's own state). The refusal must name
    /// that reason, not claim the target is outside the workspace, which
    /// here is false.
    #[cfg(unix)]
    #[test]
    fn test_resolved_symlink_target_names_an_invalid_relative_path_not_outside_the_workspace()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join(".rift"))?;
        fs::write(
            directory.path().join(".rift/state.rs"),
            "pub fn state() {}\n",
        )?;
        std::os::unix::fs::symlink(".rift/state.rs", directory.path().join("link.rs"))?;
        let reads = reads_over(directory.path(), &SourceVisibility::default())?;
        let link_absolute = directory.path().join("link.rs");
        let refusal =
            resolved_symlink_target(&reads, directory.path(), &path("link.rs"), &link_absolute)?
                .expect_err("a symlink resolving into .rift must refuse");
        let ChangeResult::Refused { diagnostics, .. } = refusal else {
            panic!("an invalid relative-path target must refuse");
        };
        assert!(
            !diagnostics[0].message.contains("outside the workspace"),
            "the resolved target is inside the workspace, so this claim is false: {diagnostics:?}"
        );
        assert!(
            diagnostics[0].message.contains("link.rs")
                && diagnostics[0].message.contains(".rift/state.rs"),
            "{diagnostics:?}"
        );
        Ok(())
    }

    /// `create_dir_all` is not atomic: a component past the filesystem's
    /// name-length limit fails only at the deepest segment, after an
    /// ancestor directory has already landed on disk. That ancestor must
    /// still be tracked for cleanup even though the call that created it
    /// returned an error.
    #[test]
    fn test_created_directories_records_a_partially_created_chain_for_cleanup() -> TestResult {
        let directory = tempfile::tempdir()?;
        let name_past_the_limit = "x".repeat(256);
        let parent = directory.path().join("outer").join(&name_past_the_limit);
        let mut created = CreatedDirectories::default();
        created
            .record(directory.path(), &parent)
            .expect_err("a path component past the name-length limit must fail create_dir_all");
        assert!(
            directory.path().join("outer").is_dir(),
            "the ancestor create_dir_all did create before failing must still be on disk"
        );
        created.remove_if_empty();
        assert!(
            !directory.path().join("outer").exists(),
            "the partially created ancestor must have been tracked and removed on cleanup"
        );
        Ok(())
    }

    /// The climb toward `root` assumes `root` is an ancestor of `parent`.
    /// When a candidate runs out of parent components before ever
    /// reaching `root`, the loop stops there instead of climbing forever,
    /// so a caller passing an unrelated `root` still gets a bounded call.
    #[test]
    fn test_created_directories_record_stops_when_parent_runs_out_of_ancestors() -> TestResult {
        let unrelated_root = tempfile::tempdir()?;
        let mut created = CreatedDirectories::default();
        created.record(unrelated_root.path(), std::path::Path::new(""))?;
        assert_eq!(
            created.0,
            vec![std::path::PathBuf::new()],
            "the climb must record the parentless candidate it stopped at before breaking"
        );
        Ok(())
    }
}
