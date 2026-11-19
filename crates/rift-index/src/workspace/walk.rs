//! Bounded parallel path selection with deterministic refusal order.

use std::path::{Path, PathBuf};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use ignore::{DirEntry, WalkBuilder, WalkState};
use rift_error::{RiftError, errors};

use super::{SOURCE_FILES_FIELD, check_cancelled, walk_error};

const SCOPE: rift_tracing::InstrumentScope =
    rift_tracing::InstrumentScope::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
static DIRECTORY_WALKS: rift_tracing::Counter<0> =
    rift_tracing::Counter::declare(SCOPE, "index.directory_walks", "{walk}", &[]);

/// Selects at most `files_max` paths, including `held` paths from an earlier walk.
///
/// Successful walks sort at the join. A refusal repeats the serial traversal so the
/// first error and the path crossing a bound stay independent of worker scheduling.
pub(super) fn selected_walk<T: Send>(
    root: &Path,
    builder: WalkBuilder,
    files_max: usize,
    held: usize,
    cancelled: &(dyn Fn() -> bool + Sync),
    select: impl Fn(&DirEntry) -> Result<Option<T>, RiftError> + Sync,
) -> Result<Vec<(PathBuf, T)>, RiftError> {
    projected_walk(
        root,
        builder,
        files_max,
        held,
        cancelled,
        select,
        |_, value| Ok(value),
    )
}

/// Projects selected entries after admission; refusal replay preserves that operation order.
pub(super) fn projected_walk<Selected: Send, Value: Send>(
    root: &Path,
    mut builder: WalkBuilder,
    files_max: usize,
    held: usize,
    cancelled: &(dyn Fn() -> bool + Sync),
    select: impl Fn(&DirEntry) -> Result<Option<Selected>, RiftError> + Sync,
    project: impl Fn(&Path, Selected) -> Result<Value, RiftError> + Sync,
) -> Result<Vec<(PathBuf, Value)>, RiftError> {
    check_cancelled(cancelled)?;
    DIRECTORY_WALKS.add(1);
    let selected = Mutex::new(Vec::new());
    let reserved = AtomicUsize::new(held);
    let refused = AtomicBool::new(false);
    builder.threads(rayon::current_num_threads());
    builder.build_parallel().run(|| {
        Box::new(|entry| {
            if cancelled() {
                return WalkState::Quit;
            }
            let projected = (|| {
                let entry = entry.map_err(|error| walk_error(root, error))?;
                let Some(value) = select(&entry)? else {
                    return Ok(None);
                };
                if reserved
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                        count.checked_add(1).filter(|next| *next <= files_max)
                    })
                    .is_err()
                {
                    return errors::index::workspace_too_many_files()
                        .path(entry.path())
                        .field(SOURCE_FILES_FIELD)
                        .observed(files_max.saturating_add(1))
                        .maximum(files_max)
                        .fail();
                }
                let value = project(entry.path(), value)?;
                Ok(Some((entry.into_path(), value)))
            })();
            match projected {
                Ok(Some(value)) => selected
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(value),
                Ok(None) => {}
                Err(_) => {
                    refused.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
            }
            WalkState::Continue
        })
    });
    check_cancelled(cancelled)?;
    if refused.load(Ordering::Relaxed) {
        return serial_projected_walk(root, &builder, files_max, held, cancelled, select, project);
    }
    let mut selected = selected
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    selected.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(selected)
}

#[cfg(test)]
fn serial_selected_walk<T>(
    root: &Path,
    builder: &WalkBuilder,
    files_max: usize,
    held: usize,
    cancelled: &(dyn Fn() -> bool + Sync),
    select: impl Fn(&DirEntry) -> Result<Option<T>, RiftError>,
) -> Result<Vec<(PathBuf, T)>, RiftError> {
    serial_projected_walk(
        root,
        builder,
        files_max,
        held,
        cancelled,
        select,
        |_, value| Ok(value),
    )
}

fn serial_projected_walk<Selected, Value>(
    root: &Path,
    builder: &WalkBuilder,
    files_max: usize,
    held: usize,
    cancelled: &(dyn Fn() -> bool + Sync),
    select: impl Fn(&DirEntry) -> Result<Option<Selected>, RiftError>,
    project: impl Fn(&Path, Selected) -> Result<Value, RiftError>,
) -> Result<Vec<(PathBuf, Value)>, RiftError> {
    DIRECTORY_WALKS.add(1);
    let mut selected = Vec::new();
    for entry in builder.build() {
        check_cancelled(cancelled)?;
        let entry = entry.map_err(|error| walk_error(root, error))?;
        let Some(value) = select(&entry)? else {
            continue;
        };
        if selected.len() >= files_max.saturating_sub(held) {
            return errors::index::workspace_too_many_files()
                .path(entry.path())
                .field(SOURCE_FILES_FIELD)
                .observed(files_max.saturating_add(1))
                .maximum(files_max)
                .fail();
        }
        let value = project(entry.path(), value)?;
        selected.push((entry.into_path(), value));
    }
    selected.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::{projected_walk, selected_walk, serial_selected_walk};
    use crate::workspace::{
        GitignorePolicy, WorkspaceIndexLimits, source_walk_builder, walked_file,
    };
    use ignore::DirEntry;
    use rift_analysis::PathMatcher;
    use rift_error::errors;
    use std::path::{Path, PathBuf};
    use std::{fs, sync::Arc};

    fn fixture() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("workspace");
        let root = directory.path();
        for name in [
            "src",
            "nested/.git",
            "ignored",
            "target",
            ".rift",
            "too/deep",
        ] {
            fs::create_dir_all(root.join(name)).expect("fixture directory");
        }
        rift_history::fixture::init(root);
        rift_history::fixture::init(&root.join("nested"));
        for (path, bytes) in [
            (".gitignore", b"ignored/\n*.tmp\n".as_slice()),
            ("src/.gitignore", b"hidden.rs\n"),
            ("nested/.gitignore", b"hidden.rs\n"),
            ("src/lib.rs", b"pub fn kept() {}"),
            ("src/hidden.rs", b"pub fn hidden() {}"),
            ("nested/kept.rs", b"pub fn nested() {}"),
            ("nested/hidden.rs", b"pub fn hidden() {}"),
            ("ignored/forced.rs", b"pub fn forced() {}"),
            ("scratch.tmp", b"ignored"),
            ("refused.rs", b"\xff\0"),
            ("large.txt", b"larger than a small file bound"),
            ("target/hidden.rs", b"pub fn hidden() {}"),
            (".rift/hidden.rs", b"pub fn hidden() {}"),
            ("too/deep/kept.rs", b"pub fn deep() {}"),
        ] {
            fs::write(root.join(path), bytes).expect("fixture file");
        }
        directory
    }

    fn compare_walks(root: &Path, gitignore: GitignorePolicy) -> Vec<PathBuf> {
        let matcher =
            Arc::new(PathMatcher::build(root, &[], &["too/**".to_owned()]).expect("source policy"));
        let builder = source_walk_builder(root, 8, gitignore, Some(matcher));
        let select = |entry: &DirEntry| walked_file(entry, 8).map(|file| file.map(|_| ()));
        let parallel =
            selected_walk(root, builder.clone(), 100, 0, &|| false, select).expect("parallel walk");
        let serial =
            serial_selected_walk(root, &builder, 100, 0, &|| false, select).expect("serial walk");
        assert_eq!(parallel, serial);
        parallel
            .into_iter()
            .map(|(path, ())| {
                path.strip_prefix(root)
                    .expect("relative path")
                    .to_path_buf()
            })
            .collect()
    }

    #[test]
    fn parallel_and_serial_walk_select_the_same_ignored_nested_and_refused_files() {
        let directory = fixture();
        let selected = compare_walks(directory.path(), GitignorePolicy::Respect);
        for kept in ["src/lib.rs", "nested/kept.rs", "refused.rs", "large.txt"] {
            assert!(
                selected.contains(&PathBuf::from(kept)),
                "{kept}: {selected:?}"
            );
        }
        for hidden in [
            "src/hidden.rs",
            "nested/hidden.rs",
            "ignored/forced.rs",
            "scratch.tmp",
            "target/hidden.rs",
            ".rift/hidden.rs",
            "too/deep/kept.rs",
        ] {
            assert!(
                !selected.contains(&PathBuf::from(hidden)),
                "{hidden}: {selected:?}"
            );
        }
        let unignored = compare_walks(directory.path(), GitignorePolicy::Ignore);
        assert!(unignored.contains(&PathBuf::from("ignored/forced.rs")));
    }

    #[cfg(unix)]
    #[test]
    fn parallel_and_serial_walk_refuse_symlinks() {
        let directory = fixture();
        let root = directory.path();
        std::os::unix::fs::symlink(root.join("src/lib.rs"), root.join("linked.rs"))
            .expect("file link");
        std::os::unix::fs::symlink(root.join("src"), root.join("linked-dir"))
            .expect("directory link");
        let selected = compare_walks(root, GitignorePolicy::Respect);
        assert!(!selected.contains(&PathBuf::from("linked.rs")));
        assert!(!selected.contains(&PathBuf::from("linked-dir/lib.rs")));
    }

    #[test]
    fn parallel_and_serial_walk_exclude_linked_worktrees() {
        use rift_history::fixture::{commit_all, git};
        let directory = fixture();
        let root = directory.path();
        commit_all(&root.join("nested"), "nested source");
        commit_all(root, "fixture source");
        git(root, &["worktree", "add", "--detach", "linked", "HEAD"]);
        let selected = compare_walks(root, GitignorePolicy::Respect);
        assert!(selected.iter().all(|path| !path.starts_with("linked")));
    }

    #[test]
    fn parallel_and_serial_force_inclusion_select_the_same_paths() {
        use crate::workspace::forced_walk;
        use rift_analysis::PathVerdict;
        let directory = fixture();
        let root = directory.path();
        let matcher = Arc::new(
            PathMatcher::build_with_force_include(
                root,
                &[],
                &[],
                &["ignored/*.rs".to_owned(), "target/**".to_owned()],
            )
            .expect("force policy"),
        );
        let builder = forced_walk(root, 8, matcher.force_include_reach(), Arc::clone(&matcher));
        let select = |entry: &DirEntry| {
            Ok(walked_file(entry, 8)?
                .filter(|path| matcher.verdict(path) == PathVerdict::ForceIncluded)
                .map(|_| ()))
        };
        let parallel = selected_walk(root, builder.clone(), 100, 0, &|| false, select)
            .expect("parallel force walk");
        let serial = serial_selected_walk(root, &builder, 100, 0, &|| false, select)
            .expect("serial force walk");
        assert_eq!(parallel, serial);
        assert_eq!(parallel.len(), 1);
        assert_eq!(parallel[0].0, root.join("ignored/forced.rs"));
    }

    #[test]
    fn parallel_projection_preserves_admission_and_error_order() {
        let directory = tempfile::tempdir().expect("projection fixture");
        let root = directory.path();
        fs::write(root.join("a.rs"), "pub fn first() {}").expect("first path");
        fs::write(root.join("b.rs"), "pub fn second() {}").expect("second path");
        let select = |entry: &DirEntry| walked_file(entry, 4).map(|path| path.map(|_| ()));
        let project = |path: &Path, ()| {
            if path.file_name() == Some(std::ffi::OsStr::new("b.rs")) {
                return errors::index::workspace_invalid_path().path(path).fail();
            }
            Ok(())
        };
        for (maximum, expected) in [
            (1, errors::index::workspace_too_many_files::SLUG),
            (2, errors::index::workspace_invalid_path::SLUG),
        ] {
            let builder = source_walk_builder(root, 4, GitignorePolicy::Ignore, None);
            let error = projected_walk(root, builder, maximum, 0, &|| false, select, project)
                .expect_err("ordered refusal");
            assert_eq!(error.slug(), expected);
        }
    }

    #[test]
    fn parallel_walk_preserves_serial_file_and_depth_refusals_and_cancellation() {
        let directory = fixture();
        let root = directory.path();
        for (depth, maximum) in [(1, 100), (8, 1)] {
            let builder = source_walk_builder(root, depth, GitignorePolicy::Ignore, None);
            let select = |entry: &DirEntry| walked_file(entry, depth).map(|path| path.map(|_| ()));
            let parallel = selected_walk(root, builder.clone(), maximum, 0, &|| false, select)
                .expect_err("walk refuses");
            let serial = serial_selected_walk(root, &builder, maximum, 0, &|| false, select)
                .expect_err("serial refuses");
            assert_eq!(parallel.slug(), serial.slug());
            assert_eq!(parallel.to_string(), serial.to_string());
        }
        let builder = source_walk_builder(root, 8, GitignorePolicy::Ignore, None);
        let error = selected_walk(
            root,
            builder,
            WorkspaceIndexLimits::default().files_max(),
            0,
            &|| true,
            |_| Ok(Some(())),
        )
        .expect_err("cancelled");
        assert_eq!(error.slug(), errors::index::workspace_cancelled::SLUG);
    }
}
