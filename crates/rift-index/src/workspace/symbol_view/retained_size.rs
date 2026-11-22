use super::{
    ReadableSymbol, SymbolDeclarations, SymbolId, TextSourceFile, WorkspaceIndex,
    WorkspaceSymbolView,
};
use get_size2::{GetSize, GetSizeTracker, StandardTracker};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

fn map<K: GetSize + Ord, V: GetSize, T: GetSizeTracker>(
    value: &BTreeMap<K, V>,
    tracker: T,
) -> (usize, T) {
    rift_protocol::retained_map_material(value, tracker, GetSize::get_heap_size_with_tracker)
}

fn arc<K: GetSize, T: GetSizeTracker>(value: &Arc<K>, mut tracker: T) -> (usize, T) {
    if !tracker.track(Arc::as_ptr(value)) {
        return (0, tracker);
    }
    let (bytes, tracker) = value.as_ref().get_heap_size_with_tracker(tracker);
    (bytes.saturating_add(std::mem::size_of::<K>()), tracker)
}

fn set<K: GetSize, T: GetSizeTracker>(value: &BTreeSet<K>, tracker: T) -> (usize, T) {
    let (bytes, tracker) = value.get_heap_size_with_tracker(tracker);
    (
        bytes.saturating_add(rift_protocol::btree_material_bytes::<K, ()>(value.len())),
        tracker,
    )
}

impl GetSize for TextSourceFile {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (path, tracker) = self.path.get_heap_size_with_tracker(tracker);
        let (content, tracker) = self.content.get_heap_size_with_tracker(tracker);
        (
            path.saturating_add(content)
                .saturating_add(2 * std::mem::size_of::<usize>()),
            tracker,
        )
    }
}

impl GetSize for ReadableSymbol {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (assembled, tracker) = self.assembled.get_heap_size_with_tracker(tracker);
        let (facts, tracker) = self.facts.get_heap_size_with_tracker(tracker);
        (assembled.saturating_add(facts), tracker)
    }
}

impl GetSize for SymbolDeclarations {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (mut bytes, tracker) = set(&self.incomplete_languages, tracker);
        let (part, tracker) = map(&self.canonical, tracker);
        bytes = bytes.saturating_add(part);
        let (part, tracker) = map(&self.provider, tracker);
        bytes = bytes.saturating_add(part);
        let (part, tracker) = map(&self.objects, tracker);
        bytes = bytes.saturating_add(part);
        let (part, mut tracker) = map(&self.facts, tracker);
        bytes = bytes.saturating_add(part);
        bytes = bytes.saturating_add(rift_protocol::btree_material_bytes::<
            SymbolId,
            Result<ReadableSymbol, rift_error::RiftError>,
        >(self.assembled.len()));
        for (identity, result) in &self.assembled {
            let (part, next) = identity.get_heap_size_with_tracker(tracker);
            tracker = next;
            bytes = bytes.saturating_add(part);
            let Ok(object) = result else {
                return (usize::MAX, tracker);
            };
            let (part, next) = object.get_heap_size_with_tracker(tracker);
            tracker = next;
            bytes = bytes.saturating_add(part);
        }
        (bytes, tracker)
    }
}

impl GetSize for WorkspaceSymbolView {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        #[cfg(test)]
        MEASUREMENTS.set(MEASUREMENTS.get() + 1);
        let (mut bytes, tracker) = self.root.get_heap_size_with_tracker(tracker);
        let (part, tracker) = self.owner.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(part);
        let (part, tracker) = map(&self.files, tracker);
        bytes = bytes.saturating_add(part);
        let (part, tracker) = map(&self.frameworks, tracker);
        bytes = bytes.saturating_add(part);
        let (part, tracker) = map(&self.text_files, tracker);
        bytes = bytes.saturating_add(part);
        let (part, tracker) = map(&self.notebooks, tracker);
        bytes = bytes.saturating_add(part);
        let (part, tracker) = arc(&self.semantics, tracker);
        bytes = bytes.saturating_add(part);
        let (part, tracker) = arc(&self.declarations, tracker);
        bytes = bytes.saturating_add(part);
        let (part, tracker) = arc(&self.documentation, tracker);
        bytes = bytes.saturating_add(part);
        let (part, tracker) = rift_protocol::retained_arc_slice_material(&self.documents, tracker);
        bytes = bytes.saturating_add(part);
        // Arc allocations are shared through the tracker; charging headers again is conservative.
        let headers = self
            .files
            .len()
            .saturating_add(self.text_files.len())
            .saturating_add(4)
            .saturating_mul(2 * std::mem::size_of::<usize>());
        (bytes.saturating_add(headers), tracker)
    }
}

impl WorkspaceSymbolView {
    /// Retained material charge, including owned capacities and conservative `BTree` syntax element storage.
    ///
    /// This charge bounds admitted material. It is not an allocator or process RSS measurement.
    /// Unsupported heap payloads and arithmetic overflow return `usize::MAX` for refusal.
    #[must_use]
    pub fn retained_material_bytes(&self) -> usize {
        self.retained_bytes
    }
}

impl WorkspaceIndex {
    /// The existing symbol view, if a read has already built it for this publication.
    #[must_use]
    pub fn cached_symbol_view(&self) -> Option<&Arc<WorkspaceSymbolView>> {
        self.symbol_view.get().and_then(|view| view.as_ref().ok())
    }

    /// Records the published allocations a retained read can share with this index.
    ///
    /// Accounts for the live index even before a read builds its cached symbol view.
    /// The tracker holds allocation identities without taking ownership of the index.
    #[must_use]
    pub fn retained_view_baseline(&self) -> StandardTracker {
        let (_, tracker) = map(&self.files, StandardTracker::default());
        let (_, tracker) = map(&self.text_files, tracker);
        let (_, tracker) = map(&self.framework_sources, tracker);
        let (_, tracker) = map(&self.raw_syntax, tracker);
        let (_, tracker) = map(&self.notebooks, tracker);
        let (_, tracker) = arc(&self.semantics, tracker);
        let (_, mut tracker) = arc(&self.documentation, tracker);
        if let Some(Ok(declarations)) = self.symbol_declarations.get() {
            (_, tracker) = arc(declarations, tracker);
        }
        for (path, file) in &self.files {
            if let Some(framework) = self.frameworks.for_source(path, &file.digest()) {
                (_, tracker) = framework.get_heap_size_with_tracker(tracker);
            }
        }
        let documents = self
            .symbol_documents
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for file in documents.values() {
            (_, tracker) = rift_protocol::retained_arc_slice_material(&file.documents, tracker);
        }
        if let Some(view) = self.cached_symbol_view() {
            (_, tracker) = arc(view, tracker);
        }
        tracker
    }
}

// The immutable factory measures all retained fields once after cache construction.
pub(super) fn measure(view: &WorkspaceSymbolView) -> usize {
    let (heap, _) = view.get_heap_size_with_tracker(StandardTracker::default());
    std::mem::size_of::<WorkspaceSymbolView>().saturating_add(heap)
}

#[cfg(test)]
std::thread_local! {
    pub(super) static MEASUREMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
