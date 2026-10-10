use super::{CachedSource, Collected, CollectedBlock, ExtractionCache};
use get_size2::{GetSize, GetSizeTracker};

impl<B: GetSize> GetSize for Collected<B> {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let mut bytes = 0_usize;
        let (field, tracker) = self.blocks.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        let (field, tracker) = self.links.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        let (field, tracker) = self.unresolved_links.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        let (field, tracker) = self.references.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        let (field, tracker) = self
            .unresolved_references
            .get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        let (field, tracker) = self.fragments.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        let (field, tracker) = self.candidates.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        let (field, tracker) = self.warnings.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        (bytes, tracker)
    }
}
impl GetSize for CollectedBlock {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let mut bytes = 0_usize;
        let (field, tracker) = self.identity.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        let (field, tracker) = self.content_digest.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        let (field, tracker) = self.heading_path.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        let (field, tracker) = self.language.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        let (field, tracker) = self.symbol.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        (bytes, tracker)
    }
}
impl GetSize for CachedSource {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let mut bytes = 0_usize;
        let (field, tracker) = self.key.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        let (field, tracker) = self.facts.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        (
            bytes.saturating_add(2 * std::mem::size_of::<usize>()),
            tracker,
        )
    }
}
impl GetSize for ExtractionCache {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (bytes, tracker) = self.sources.get_heap_size_with_tracker(tracker);
        let nodes = rift_protocol::btree_material_bytes::<
            rift_protocol::documentation::DocumentationContentIdentity,
            CachedSource,
        >(self.sources.len());
        (bytes.saturating_add(nodes), tracker)
    }
}
