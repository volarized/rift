//! The working tree compared against one committed revision, for a `change`
//! whose `head` is the working tree.
//!
//! gix's `status` lists the paths: the base tree against the index, then the
//! index against the working tree. The base side reads its files in the form
//! the working tree holds them. Neither pass runs an external `filter` driver
//! or writes into the repository: both read a copy of the configuration with
//! every `filter` section removed, so a driver such as git-lfs never starts
//! and never stores an object under `.git/lfs`.

use std::collections::BTreeMap;
use std::io::Read as _;

use gix::bstr::ByteSlice as _;
use rift_error::RiftError;
use rift_error::errors;
use sha2::{Digest as _, Sha256};

use crate::repository::{
    ChangedFiles, Repository, ResolvedRevision, TreeFile, strip_workspace_prefix,
};

/// The `filter` attribute value git-lfs registers.
const LFS_DRIVER: &str = "lfs";

/// The attribute naming a path's external driver.
const FILTER_ATTRIBUTE: &str = "filter";

/// The attribute naming the encoding a path's working file holds.
const WORKING_TREE_ENCODING_ATTRIBUTE: &str = "working-tree-encoding";

/// The prefix every UTF-16 encoding name carries, `UTF-16LE` and `UTF-16BE` alike.
const UTF16_ENCODING_PREFIX: &str = "UTF-16";

/// The most bytes an LFS pointer may hold for the check to read it whole: a
/// git-lfs spec v1 pointer is about 130 bytes.
const LFS_POINTER_BYTES_MAX: u64 = 1024;

/// The line of an LFS pointer that records the content's length.
const LFS_POINTER_SIZE_PREFIX: &[u8] = b"size ";

/// Bytes the working-file checks read at once while hashing.
const READ_CHUNK_BYTES: usize = 64 << 10;

/// One committed file in the form the working tree holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkingForm {
    /// The committed bytes after the built-in conversions alone: `eol`,
    /// `ident`, and `working-tree-encoding`.
    Converted(Vec<u8>),
    /// The path's attributes name a `filter` driver, whose smudge command a
    /// read never runs.
    Filtered {
        /// The driver the `filter` attribute names.
        driver: String,
    },
    /// The path's `working-tree-encoding` names UTF-16, which the built-in
    /// conversion does not write back.
    Encoded {
        /// The encoding the attribute names.
        encoding: String,
    },
}

impl Repository {
    /// Lists the paths whose working-tree bytes differ from `base`'s tree,
    /// workspace-relative, passing `includes`, sorted.
    ///
    /// A path only the base-to-index pass reports is changed: the working file
    /// matches the index. A path the index-to-working-tree pass reports is
    /// checked once more against the base blob, since a staged edit reverted in
    /// the working tree, and an LFS file whose stat moved, reach that pass with
    /// bytes equal to the base. A path whose attributes name another `filter`
    /// driver stays changed: its clean command never runs.
    ///
    /// `status` walks no directory and tracks no rename: a path of
    /// `published`, the paths the current index read, that git's index does
    /// not hold is untracked and changed, so git's own excludes never hide a
    /// file every current-tree read serves, and a moved file is one deletion
    /// beside one addition.
    ///
    /// `status` compares every tracked path before the first one is listed, so
    /// `paths_max` bounds the answer and the base checks, not that pass.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for an unreadable object store, index, or
    /// attribute file.
    pub fn changed_working_files(
        &self,
        base: &ResolvedRevision,
        published: &[&str],
        includes: &dyn Fn(&str) -> bool,
        paths_max: usize,
    ) -> Result<ChangedFiles, RiftError> {
        let repository = self.driver_free()?;
        let base_tree = self.commit_tree(base)?;
        let mut reported = self.status_paths(&repository, &base_tree, includes)?;
        let index = repository.index_or_empty().map_err(|error| {
            errors::history::storage()
                .operation("read index")
                .detail(&error)
                .error()
        })?;
        for path in published {
            let tracked = index
                .entry_by_path(repository_path(&self.prefix, path).as_bytes().as_bstr())
                .is_some();
            if !tracked && includes(path) {
                reported.entry((*path).to_owned()).or_default();
            }
        }
        let mut checks = BaseChecks::new(&repository, &base_tree, &self.prefix)?;
        let mut paths = Vec::new();
        let mut truncated = false;
        for (path, pass) in reported {
            if pass == ReportedBy::IndexToWorktree && checks.working_bytes_match_base(&path)? {
                continue;
            }
            if paths.len() == paths_max {
                truncated = true;
                break;
            }
            paths.push(path);
        }
        Ok(ChangedFiles { paths, truncated })
    }

    /// The workspace-relative paths `status` reports against `base_tree`,
    /// each with the pass that reported it last in the pass order.
    fn status_paths(
        &self,
        repository: &gix::Repository,
        base_tree: &gix::Tree<'_>,
        includes: &dyn Fn(&str) -> bool,
    ) -> Result<BTreeMap<String, ReportedBy>, RiftError> {
        let items = repository
            .status(gix::progress::Discard)
            .map_err(|error| {
                errors::history::storage()
                    .operation("start status")
                    .detail(&error)
                    .error()
            })?
            .untracked_files(gix::status::UntrackedFiles::None)
            .index_worktree_submodules(None)
            .index_worktree_rewrites(None)
            .head_tree(base_tree.id)
            .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
            .into_iter(Vec::<gix::bstr::BString>::new())
            .map_err(|error| {
                errors::history::storage()
                    .operation("start status")
                    .detail(&error)
                    .error()
            })?;
        let mut reported: BTreeMap<String, ReportedBy> = BTreeMap::new();
        for item in items {
            let item = item.map_err(|error| {
                errors::history::storage()
                    .operation("compare working tree")
                    .detail(&error)
                    .error()
            })?;
            let Some(path) = strip_workspace_prefix(item.location(), self.prefix.as_bytes())
                .and_then(|relative| std::str::from_utf8(relative).ok())
                .filter(|relative| includes(relative))
            else {
                continue;
            };
            let pass = match item {
                gix::status::Item::IndexWorktree(_) => ReportedBy::IndexToWorktree,
                gix::status::Item::TreeIndex(_) => ReportedBy::BaseToIndex,
            };
            let entry = reported.entry(path.to_owned()).or_default();
            *entry = (*entry).max(pass);
        }
        Ok(reported)
    }

    /// The converter that reads committed files in the working form, with the
    /// pipeline's external drivers cleared.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for an unreadable index or attribute file.
    pub fn working_forms(&self) -> Result<WorkingForms<'_>, RiftError> {
        let repository = self.driver_free()?;
        let (pipeline, _index) = repository.filter_pipeline(None).map_err(|error| {
            errors::history::storage()
                .operation("build filter pipeline")
                .detail(&error)
                .error()
        })?;
        let (mut pipeline, attributes) = pipeline.into_parts();
        pipeline.options_mut().drivers.clear();
        let selected = attributes
            .selected_attribute_matches([FILTER_ATTRIBUTE, WORKING_TREE_ENCODING_ATTRIBUTE]);
        Ok(WorkingForms {
            source: self,
            repository,
            pipeline,
            attributes,
            selected,
        })
    }

    /// A copy of this repository whose configuration holds no `filter`
    /// section, so no pipeline built from it knows an external driver.
    fn driver_free(&self) -> Result<gix::Repository, RiftError> {
        let mut repository = self.inner.clone();
        let mut snapshot = repository.config_snapshot_mut();
        let names: Vec<Option<gix::bstr::BString>> = snapshot
            .sections_by_name(FILTER_ATTRIBUTE)
            .map(|sections| {
                sections
                    .map(|section| section.header().subsection_name().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        for name in &names {
            while snapshot
                .remove_section(FILTER_ATTRIBUTE, name.as_ref().map(|name| name.as_bstr()))
                .is_some()
            {}
        }
        snapshot.commit().map_err(|error| {
            errors::history::storage()
                .operation("drop filter configuration")
                .detail(&error)
                .error()
        })?;
        Ok(repository)
    }
}

/// Which `status` pass reported a path. The index-to-working-tree pass
/// orders last, so a path both passes report keeps it and meets the base
/// check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum ReportedBy {
    /// The base tree against git's index, or a file git's index does not
    /// hold: the working bytes are what the index holds.
    #[default]
    BaseToIndex,
    /// Git's index against the working tree: the working bytes may still
    /// equal the base blob.
    IndexToWorktree,
}

/// Committed files read in the working form: one attribute lookup and one
/// built-in conversion pipeline, reused across the paths one comparison reads.
pub struct WorkingForms<'source> {
    source: &'source Repository,
    repository: gix::Repository,
    pipeline: gix::filter::plumbing::Pipeline,
    attributes: gix::worktree::Stack,
    selected: gix::attrs::search::Outcome,
}

impl std::fmt::Debug for WorkingForms<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkingForms")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl WorkingForms<'_> {
    /// `file` at its revision in the working form. A path whose attributes
    /// name a `filter` driver or a UTF-16 `working-tree-encoding` answers
    /// that attribute without reading the blob.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] for a blob past `bytes_max`, an unreadable
    /// object store or attribute file, or a conversion that fails.
    pub fn form(&mut self, file: &TreeFile, bytes_max: usize) -> Result<WorkingForm, RiftError> {
        let repository_path = repository_path(&self.source.prefix, &file.path);
        let platform = self
            .attributes
            .at_entry(repository_path.as_str(), None, &self.repository.objects)
            .map_err(|error| {
                errors::history::storage()
                    .operation("read attributes")
                    .detail(&error)
                    .error()
            })?;
        self.selected.reset();
        platform.matching_attributes(&mut self.selected);
        if let Some(driver) = attribute_value(&self.selected, FILTER_ATTRIBUTE) {
            return Ok(WorkingForm::Filtered { driver });
        }
        if let Some(encoding) = attribute_value(&self.selected, WORKING_TREE_ENCODING_ATTRIBUTE)
            .filter(|encoding| {
                encoding
                    .to_ascii_uppercase()
                    .starts_with(UTF16_ENCODING_PREFIX)
            })
        {
            return Ok(WorkingForm::Encoded { encoding });
        }
        let bytes = self.source.blob_bytes(file, bytes_max)?;
        let converted = self
            .pipeline
            .convert_to_worktree(
                &bytes,
                repository_path.as_str().into(),
                &mut |_, outcome| {
                    platform.matching_attributes(outcome);
                },
                gix::filter::plumbing::pipeline::convert::to_worktree::Options::default(),
            )
            .map_err(|error| {
                errors::history::storage()
                    .operation("convert to working form")
                    .detail(&error)
                    .error()
            })?;
        let converted = converted.as_bytes().ok_or_else(|| {
            errors::history::storage()
                .operation("convert to working form")
                .detail(&"a driver answered")
                .error()
        })?;
        Ok(WorkingForm::Converted(converted.to_vec()))
    }
}

/// The attribute lookup and clean conversion one comparison reuses across the
/// paths it checks against the base tree.
struct BaseChecks<'repo> {
    repository: &'repo gix::Repository,
    base_tree: &'repo gix::Tree<'repo>,
    prefix: &'repo str,
    attributes: gix::AttributeStack<'repo>,
    filter: gix::attrs::search::Outcome,
    pipeline: gix::filter::Pipeline<'repo>,
    index: gix::worktree::IndexPersistedOrInMemory,
    workdir: std::path::PathBuf,
}

impl<'repo> BaseChecks<'repo> {
    fn new(
        repository: &'repo gix::Repository,
        base_tree: &'repo gix::Tree<'repo>,
        prefix: &'repo str,
    ) -> Result<Self, RiftError> {
        let (pipeline, index) = repository.filter_pipeline(None).map_err(|error| {
            errors::history::storage()
                .operation("build filter pipeline")
                .detail(&error)
                .error()
        })?;
        let attributes = repository
            .attributes_only(
                &index,
                gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
            )
            .map_err(|error| {
                errors::history::storage()
                    .operation("read attributes")
                    .detail(&error)
                    .error()
            })?;
        let filter = attributes.selected_attribute_matches([FILTER_ATTRIBUTE]);
        let workdir = repository
            .workdir()
            .ok_or_else(|| {
                errors::history::storage()
                    .operation("read working tree")
                    .detail(&"the repository is bare")
                    .error()
            })?
            .to_owned();
        Ok(Self {
            repository,
            base_tree,
            prefix,
            attributes,
            filter,
            pipeline,
            index,
            workdir,
        })
    }

    /// Whether the working file at `path` holds the base blob's bytes once in
    /// committed form: the clean conversion for a plain path, the computed
    /// git-lfs pointer for an LFS path. Another driver's path never matches.
    fn working_bytes_match_base(&mut self, path: &str) -> Result<bool, RiftError> {
        let repository_path = repository_path(self.prefix, path);
        let Some(base) = self
            .base_tree
            .lookup_entry_by_path(&repository_path)
            .map_err(|error| {
                errors::history::storage()
                    .operation("read base tree")
                    .detail(&error)
                    .error()
            })?
            .filter(|entry| entry.mode().is_blob())
        else {
            return Ok(false);
        };
        let Ok(file) = std::fs::File::open(self.workdir.join(&repository_path)) else {
            return Ok(false);
        };
        self.filter.reset();
        self.attributes
            .at_entry(repository_path.as_str(), None)
            .map_err(|error| {
                errors::history::storage()
                    .operation("read attributes")
                    .detail(&error)
                    .error()
            })?
            .matching_attributes(&mut self.filter);
        match attribute_value(&self.filter, FILTER_ATTRIBUTE).as_deref() {
            Some(LFS_DRIVER) => self.lfs_file_matches_base(file, base.object_id()),
            Some(_) => Ok(false),
            None => self.clean_file_matches_base(file, &repository_path, base.object_id()),
        }
    }

    /// Whether a plain working file, cleaned by the built-in conversions,
    /// hashes to `base`. The read stops one byte past the base blob's length,
    /// so a file of another length answers without being read whole.
    fn clean_file_matches_base(
        &mut self,
        file: std::fs::File,
        repository_path: &str,
        base: gix::ObjectId,
    ) -> Result<bool, RiftError> {
        let base_size = self
            .repository
            .find_header(base)
            .map_err(|error| {
                errors::history::storage()
                    .operation("read blob header")
                    .detail(&error)
                    .error()
            })?
            .size();
        let cleaned = self
            .pipeline
            .convert_to_git(file, std::path::Path::new(repository_path), &self.index)
            .map_err(|error| {
                errors::history::storage()
                    .operation("convert to committed form")
                    .detail(&error)
                    .error()
            })?;
        let working = blob_id_of_length(cleaned, base_size, base.kind())?;
        Ok(working == Some(base))
    }

    /// Whether an LFS working file matches the base blob: byte for byte when
    /// the file was left as a pointer, else through the git-lfs spec v1
    /// pointer the check computes itself. A base pointer whose `size` line
    /// names another length answers without hashing the file.
    fn lfs_file_matches_base(
        &self,
        mut file: std::fs::File,
        base: gix::ObjectId,
    ) -> Result<bool, RiftError> {
        let size = file
            .metadata()
            .map_err(|error| {
                errors::history::storage()
                    .operation("read working file")
                    .detail(&error)
                    .error()
            })?
            .len();
        let base_size = self
            .repository
            .find_header(base)
            .map_err(|error| {
                errors::history::storage()
                    .operation("read blob header")
                    .detail(&error)
                    .error()
            })?
            .size();
        if base_size <= LFS_POINTER_BYTES_MAX {
            let pointer = self.repository.find_object(base).map_err(|error| {
                errors::history::storage()
                    .operation("read blob")
                    .detail(&error)
                    .error()
            })?;
            if pointer.data.as_slice() == read_small(&mut file, size)?.as_slice() {
                return Ok(true);
            }
            if pointer_size(&pointer.data).is_some_and(|recorded| recorded != size) {
                return Ok(false);
            }
        }
        rewind(&mut file)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher).map_err(|error| {
            errors::history::storage()
                .operation("read working file")
                .detail(&error)
                .error()
        })?;
        Ok(pointer_id(&hasher.finalize(), size, base.kind())? == base)
    }
}

/// The bytes of a file no longer than [`LFS_POINTER_BYTES_MAX`]; empty for a
/// longer one, which no pointer equals.
fn read_small(file: &mut std::fs::File, size: u64) -> Result<Vec<u8>, RiftError> {
    if size > LFS_POINTER_BYTES_MAX {
        return Ok(Vec::new());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| {
        errors::history::storage()
            .operation("read working file")
            .detail(&error)
            .error()
    })?;
    Ok(bytes)
}

/// Moves `file`'s cursor back to its first byte.
fn rewind(file: &mut std::fs::File) -> Result<(), RiftError> {
    use std::io::Seek as _;
    file.rewind().map_err(|error| {
        errors::history::storage()
            .operation("read working file")
            .detail(&error)
            .error()
    })
}

/// The blob id of the bytes `stream` yields, when it yields exactly
/// `length` of them; `None` for any other length. The stream is read in
/// chunks and at most one byte past `length`.
fn blob_id_of_length(
    stream: impl std::io::Read,
    length: u64,
    kind: gix::hash::Kind,
) -> Result<Option<gix::ObjectId>, RiftError> {
    let mut hasher = gix::hash::hasher(kind);
    hasher.update(&gix::objs::encode::loose_header(
        gix::objs::Kind::Blob,
        length,
    ));
    let mut bounded = stream.take(length.saturating_add(1));
    let mut chunk = vec![0_u8; READ_CHUNK_BYTES];
    let mut read_total = 0_u64;
    loop {
        let read = bounded.read(&mut chunk).map_err(|error| {
            errors::history::storage()
                .operation("read working file")
                .detail(&error)
                .error()
        })?;
        if read == 0 {
            break;
        }
        read_total = read_total.saturating_add(read as u64);
        hasher.update(&chunk[..read]);
    }
    if read_total != length {
        return Ok(None);
    }
    hasher.try_finalize().map(Some).map_err(|error| {
        errors::history::storage()
            .operation("hash working file")
            .detail(&error)
            .error()
    })
}

/// The blob id of the git-lfs spec v1 pointer for content with the SHA-256
/// `digest` and `size`: `version`, `oid sha256:<hex>`, and `size <bytes>`,
/// one per line.
fn pointer_id(
    digest: &sha2::digest::Output<Sha256>,
    size: u64,
    kind: gix::hash::Kind,
) -> Result<gix::ObjectId, RiftError> {
    let pointer =
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{digest:x}\nsize {size}\n");
    gix::objs::compute_hash(kind, gix::objs::Kind::Blob, pointer.as_bytes()).map_err(|error| {
        errors::history::storage()
            .operation("hash lfs pointer")
            .detail(&error)
            .error()
    })
}

/// The length an LFS pointer's `size` line records.
fn pointer_size(pointer: &[u8]) -> Option<u64> {
    pointer
        .lines()
        .find_map(|line| line.strip_prefix(LFS_POINTER_SIZE_PREFIX))
        .and_then(|value| value.to_str().ok()?.trim().parse().ok())
}

/// The value the selected attribute `name` is set to, if it is set to one.
fn attribute_value(selected: &gix::attrs::search::Outcome, name: &str) -> Option<String> {
    selected
        .iter_selected()
        .filter(|matched| matched.assignment.name.as_str() == name)
        .find_map(|matched| match matched.assignment.state {
            gix::attrs::StateRef::Value(value) => Some(value.as_bstr().to_str_lossy().into_owned()),
            _ => None,
        })
}

/// The repository-relative spelling of a workspace-relative `path`.
fn repository_path(prefix: &str, path: &str) -> String {
    if prefix.is_empty() {
        path.to_owned()
    } else {
        format!("{prefix}/{path}")
    }
}

#[cfg(test)]
mod tests;
