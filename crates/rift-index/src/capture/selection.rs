//! Metadata observations that invalidate a retained path selection.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};

use super::{CaptureBoundary, FileStat};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputKind {
    Directory,
    Policy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObservedValue {
    Missing,
    Directory,
    Metadata(FileStat),
}

impl InputKind {
    fn read(self, path: &Path) -> Option<ObservedValue> {
        let read = match self {
            Self::Directory => fs::symlink_metadata(path),
            Self::Policy => fs::metadata(path),
        };
        match (self, read) {
            (Self::Directory, Ok(metadata)) if metadata.is_dir() => {
                Some(ObservedValue::Metadata(FileStat::of(&metadata)))
            }
            (Self::Policy, Ok(metadata)) if metadata.is_file() => {
                Some(ObservedValue::Metadata(FileStat::of(&metadata)))
            }
            (Self::Policy, Ok(metadata)) if metadata.is_dir() => Some(ObservedValue::Directory),
            (Self::Policy, Err(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                Some(ObservedValue::Missing)
            }
            _ => None,
        }
    }
}

/// Bounded observations gathered by discovery; overflow disables selection reuse.
#[derive(Debug)]
pub(crate) struct DiscoveryInputs {
    maximum: usize,
    paths: Mutex<BTreeMap<PathBuf, (InputKind, ObservedValue)>>,
    valid: AtomicBool,
}

impl DiscoveryInputs {
    pub(crate) fn new(maximum: usize) -> Self {
        Self {
            maximum,
            paths: Mutex::new(BTreeMap::new()),
            valid: AtomicBool::new(true),
        }
    }

    pub(crate) fn directory(&self, path: &Path) {
        self.observe(path, InputKind::Directory);
    }

    /// Includes policy files the ignore walker can read before filtering entries.
    pub(crate) fn ignore_directory(&self, path: &Path) {
        self.directory(path);
        self.present_policy(&path.join(".gitignore"));
        let marker = path.join(".git");
        if !self.present_policy(&marker) {
            return;
        }
        if marker.is_dir() {
            self.policy(&marker.join("info/exclude"));
            self.policy(&marker.join("commondir"));
        } else if marker.is_file()
            && let Ok(repository) = rift_history::Repository::open(path)
        {
            self.policy(&repository.exclude_path());
            self.policy(&repository.index_lock_path().with_file_name("commondir"));
        }
    }

    /// The observed parent directory detects an absent entry appearing. Existing links
    /// still record missing targets, since a target can appear outside that directory.
    fn present_policy(&self, path: &Path) -> bool {
        if !self.valid.load(Ordering::Relaxed) {
            return false;
        }
        match fs::symlink_metadata(path) {
            Ok(_) => {
                self.policy(path);
                true
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                false
            }
            Err(_) => {
                self.valid.store(false, Ordering::Relaxed);
                false
            }
        }
    }

    pub(crate) fn policy(&self, path: &Path) {
        self.observe(path, InputKind::Policy);
    }

    fn observe(&self, path: &Path, kind: InputKind) {
        if !self.valid.load(Ordering::Relaxed) {
            return;
        }
        let Some(stat) = kind.read(path) else {
            self.valid.store(false, Ordering::Relaxed);
            return;
        };
        let mut paths = self
            .paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.valid.load(Ordering::Relaxed) {
            return;
        }
        if let Some(previous) = paths.get(path) {
            if *previous != (kind, stat) {
                self.valid.store(false, Ordering::Relaxed);
            }
            return;
        }
        if paths.len() >= self.maximum {
            self.valid.store(false, Ordering::Relaxed);
            return;
        }
        paths.insert(path.to_path_buf(), (kind, stat));
    }

    pub(crate) fn finish(self) -> Option<Arc<SelectionInputs>> {
        if !self.valid.load(Ordering::Relaxed) {
            return None;
        }
        let paths = self
            .paths
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(Arc::new(SelectionInputs {
            paths: paths.into_iter().collect(),
        }))
    }
}

/// Directory and ignore-file metadata captured by one complete discovery.
#[derive(Debug)]
pub(crate) struct SelectionInputs {
    paths: Vec<(PathBuf, (InputKind, ObservedValue))>,
}

impl SelectionInputs {
    pub(crate) fn unchanged(
        &self,
        boundary: Option<CaptureBoundary>,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> bool {
        let Some(boundary) = boundary else {
            return false;
        };
        self.paths.par_iter().all(|(path, (kind, stat))| {
            let old = match stat {
                ObservedValue::Metadata(stat) => boundary.proves_old_file(*stat),
                ObservedValue::Missing | ObservedValue::Directory => true,
            };
            !cancelled() && old && kind.read(path) == Some(*stat)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::DiscoveryInputs;
    use std::fs;

    #[test]
    fn observation_overflow_disables_reuse_and_missing_policy_is_recorded() {
        let root = tempfile::tempdir().expect("workspace");
        let full = DiscoveryInputs::new(0);
        full.directory(root.path());
        full.directory(root.path());
        assert!(full.finish().is_none());
        let missing = DiscoveryInputs::new(2);
        let policy = root.path().join("missing");
        missing.policy(&policy);
        let missing = missing.finish().expect("absence is a policy observation");
        assert_eq!(missing.paths.len(), 1);
        fs::write(&policy, "new policy\n").expect("new policy");
        assert_ne!(
            missing.paths[0].1.0.read(&policy),
            Some(missing.paths[0].1.1)
        );
    }

    #[test]
    fn repeated_policy_observation_requires_matching_metadata() {
        let root = tempfile::tempdir().expect("workspace");
        let path = root.path().join(".gitignore");
        fs::write(&path, "old\n").expect("policy");
        let unchanged = DiscoveryInputs::new(2);
        unchanged.directory(root.path());
        unchanged.policy(&path);
        unchanged.policy(&path);
        assert_eq!(
            unchanged
                .finish()
                .expect("complete observations")
                .paths
                .len(),
            2
        );
        let moved = DiscoveryInputs::new(2);
        moved.policy(&path);
        fs::write(&path, "changed policy\n").expect("policy edit");
        moved.policy(&path);
        assert!(moved.finish().is_none());
    }
}
