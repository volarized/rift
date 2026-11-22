use std::collections::BTreeMap;
use std::sync::Arc;

use rift_core::FileDigest;
use rift_syntax::{ByteRange, DocumentPlacement, SyntaxFacts};
use sha2::{Digest as _, Sha256};

use crate::{ArchiveMemberKind, IndexedFile, NamespaceInput};

/// Slots retained by one complete project capture, including unrecorded calls.
pub(super) const RUST_CALL_SLOTS_MAX: usize = 131_072;
pub(super) const RUST_CALL_FILES_MAX: usize = 4_096;
pub(super) const RUST_FILE_CALL_SLOTS_MAX: usize = 8_192;
const RUST_PLACEMENT_ROWS_MAX: usize = 131_072;

#[derive(Debug)]
pub(crate) struct RustCallCapture {
    context: FileDigest,
    sources: Arc<[Arc<SyntaxFacts>]>,
    previous: Option<Arc<RustCallCache>>,
    placement_rows: usize,
}

#[derive(Debug)]
pub(crate) struct RustCallCache {
    context: FileDigest,
    sources: Arc<[Arc<SyntaxFacts>]>,
    files: BTreeMap<usize, Arc<[Option<RustCall>]>>,
}

impl RustCallCapture {
    pub(crate) fn new<'source, 'placement>(
        input: NamespaceInput<'_>,
        selected: impl IntoIterator<Item = (&'source IndexedFile, &'placement DocumentPlacement)>,
        previous: Option<&Arc<RustCallCache>>,
    ) -> Option<Self> {
        if !input.dependencies().is_empty() || input.rust_configuration().is_some() {
            return None;
        }
        let mut selected = selected
            .into_iter()
            .filter(|(file, _)| file.syntax().language().name == "rust")
            .peekable();
        selected.peek()?;
        let mut hasher = Sha256::new();
        let owner = input.owner().local_scope().ok()?;
        hash_field(&mut hasher, owner.as_bytes());
        hasher.update(input.syntax().source_bytes_max().to_be_bytes());
        hasher.update(input.syntax().syntax_elements_max().to_be_bytes());
        hasher.update(input.syntax().syntax_depth_max().to_be_bytes());
        for source in input.files().iter().chain(input.context_sources()) {
            let path = source.path().as_str();
            if path == "Cargo.toml" || path.ends_with("/Cargo.toml") {
                hasher.update([1]);
                hash_field(&mut hasher, path.as_bytes());
                hash_field(&mut hasher, source.text().as_bytes());
            }
        }
        hasher.update([0, u8::from(input.build_paths().is_some())]);
        if let Some(paths) = input.build_paths() {
            hasher.update(paths.len().to_be_bytes());
            for (path, kind) in paths {
                hash_field(&mut hasher, path.0.as_bytes());
                hasher.update([match kind {
                    None => 0,
                    Some(ArchiveMemberKind::File) => 1,
                    Some(ArchiveMemberKind::Directory) => 2,
                    Some(ArchiveMemberKind::Link) => 3,
                }]);
            }
        }
        let mut sources = Vec::new();
        let mut rows = 0_usize;
        for (file, placement) in selected {
            let syntax = file.syntax();
            rows = rows
                .checked_add(syntax.symbols().len())?
                .checked_add(placement.aliases().len())?;
            if sources.len() >= RUST_CALL_FILES_MAX || rows > RUST_PLACEMENT_ROWS_MAX {
                return None;
            }
            sources.push(Arc::clone(file.syntax_facts()));
            hash_field(&mut hasher, file.path().as_str().as_bytes());
            hasher.update(file.digest().as_bytes());
            hash_field(&mut hasher, placement.unit().as_str().as_bytes());
            hash_field(&mut hasher, placement.identity_path().as_bytes());
            hasher.update(syntax.symbols().len().to_be_bytes());
            for symbol in syntax.symbols() {
                hash_optional(
                    &mut hasher,
                    placement
                        .logical_identity(&symbol.qualified_name)
                        .map(|id| id.as_str()),
                );
            }
            hasher.update(placement.aliases().len().to_be_bytes());
            for alias in placement.aliases() {
                hash_field(&mut hasher, alias.name.as_bytes());
                hash_field(&mut hasher, alias.qualified_name.as_bytes());
                hash_field(&mut hasher, alias.identity.as_str().as_bytes());
                hash_field(&mut hasher, alias.target_identity.as_bytes());
                hasher.update(alias.range.start.to_be_bytes());
                hasher.update(alias.range.end.to_be_bytes());
                hash_optional(&mut hasher, alias.visibility.as_deref());
            }
        }
        let context = FileDigest::from_bytes(hasher.finalize().into());
        let previous = previous
            .filter(|cache| {
                cache.context == context
                    && cache.sources.len() == sources.len()
                    && sources
                        .iter()
                        .zip(cache.sources.iter())
                        .all(|(current, prior)| Arc::ptr_eq(current, prior))
            })
            .map(Arc::clone);
        let sources = previous
            .as_ref()
            .map_or_else(|| sources.into(), |cache| Arc::clone(&cache.sources));
        Some(Self {
            context,
            sources,
            previous,
            placement_rows: rows,
        })
    }

    pub(super) fn files(&self) -> usize {
        self.sources.len()
    }

    pub(super) const fn placement_rows(&self) -> usize {
        self.placement_rows
    }

    pub(super) fn file(&self, file: usize, slots: usize) -> RustCallRun {
        if let Some(calls) = self
            .previous
            .as_ref()
            .and_then(|previous| previous.files.get(&file))
            && calls.len() == slots
        {
            return RustCallRun::Replay(Arc::clone(calls));
        }
        RustCallRun::Record {
            slots,
            calls: Vec::new(),
        }
    }

    pub(super) fn finish(
        &self,
        files: BTreeMap<usize, Arc<[Option<RustCall>]>>,
    ) -> Arc<RustCallCache> {
        Arc::new(RustCallCache {
            context: self.context,
            sources: Arc::clone(&self.sources),
            files,
        })
    }
}

fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(bytes.len().to_be_bytes());
    hasher.update(bytes);
}

fn hash_optional(hasher: &mut Sha256, value: Option<&str>) {
    hasher.update([u8::from(value.is_some())]);
    if let Some(value) = value {
        hash_field(hasher, value.as_bytes());
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RustCall {
    pub(super) charged: usize,
    pub(super) target_file: usize,
    pub(super) target_range: ByteRange,
}

#[derive(Debug, Default)]
pub(super) enum RustCallRun {
    #[default]
    Disabled,
    Record {
        slots: usize,
        calls: Vec<Option<RustCall>>,
    },
    Replay(Arc<[Option<RustCall>]>),
}

impl RustCallRun {
    pub(super) fn lookup(&self, slot: usize) -> Option<RustCall> {
        match self {
            Self::Replay(calls) => calls.get(slot).copied().flatten(),
            _ => None,
        }
    }

    pub(super) fn record(&mut self, slot: usize, call: RustCall) {
        if let Self::Record { slots, calls } = self {
            if calls.is_empty() {
                calls.resize(*slots, None);
            }
            calls[slot] = Some(call);
        }
    }

    pub(super) fn finish(self) -> Option<Arc<[Option<RustCall>]>> {
        match self {
            Self::Record { calls, .. } => (!calls.is_empty()).then(|| calls.into()),
            Self::Replay(calls) => Some(calls),
            Self::Disabled => None,
        }
    }
}

impl get_size2::GetSize for RustCallCache {
    fn get_heap_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (sources, tracker) =
            rift_protocol::retained_arc_material(&self.sources, tracker, |sources, tracker| {
                // The syntax is also owned by the captured files. Use the same tracker so a
                // shared allocation is charged once regardless of which owner is measured first.
                sources
                    .iter()
                    .fold((0_usize, tracker), |(bytes, tracker), source| {
                        let (allocation, tracker) =
                            get_size2::GetSize::get_heap_size_with_tracker(source, tracker);
                        (bytes.saturating_add(allocation), tracker)
                    })
            });
        let (files, tracker) =
            rift_protocol::retained_map_material(&self.files, tracker, |calls, tracker| {
                rift_protocol::retained_arc_material(calls, tracker, |_, tracker| (0, tracker))
            });
        (sources.saturating_add(files), tracker)
    }
}
