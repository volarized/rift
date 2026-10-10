use super::{DocumentationCollection, DocumentationContentIdentity, DocumentationDigest, SymbolId};
use get_size2::{GetSize, GetSizeTracker};

impl GetSize for DocumentationCollection {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (bytes, tracker) = self.index.get_heap_size_with_tracker(tracker);
        let (references, tracker) = self.references.get_heap_size_with_tracker(tracker);
        let (blocks, tracker) = self.blocks.get_heap_size_with_tracker(tracker);
        let (sources, tracker) = self.sources.get_heap_size_with_tracker(tracker);
        let mut bytes = bytes
            .saturating_add(references)
            .saturating_add(blocks)
            .saturating_add(sources);
        bytes = bytes.saturating_add(rift_protocol::btree_material_bytes::<SymbolId, Vec<usize>>(
            self.references.len(),
        ));
        bytes = bytes.saturating_add(rift_protocol::btree_material_bytes::<
            DocumentationDigest,
            usize,
        >(self.blocks.len()));
        bytes = bytes.saturating_add(rift_protocol::btree_material_bytes::<
            DocumentationContentIdentity,
            usize,
        >(self.sources.len()));
        #[cfg(feature = "collector")]
        let (bytes, tracker) = {
            let (extraction, tracker) = self.extraction_cache.get_heap_size_with_tracker(tracker);
            let (resolution, tracker) = self.resolution_cache.get_heap_size_with_tracker(tracker);
            (
                bytes.saturating_add(extraction).saturating_add(resolution),
                tracker,
            )
        };
        (bytes, tracker)
    }
}
