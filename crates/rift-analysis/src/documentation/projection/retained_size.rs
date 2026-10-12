use super::{BlockAddress, DocumentationLayer, HeldCollection, Mapping};
use get_size2::{GetSize, GetSizeTracker};

impl GetSize for HeldCollection<'_> {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Shared(value) => {
                let (bytes, tracker) = value.get_heap_size_with_tracker(tracker);
                (
                    bytes.saturating_add(2 * std::mem::size_of::<usize>()),
                    tracker,
                )
            }
            Self::Borrowed(_) => (0, tracker),
        }
    }
}

impl GetSize for BlockAddress {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }
}
impl GetSize for Mapping {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }
}

impl GetSize for DocumentationLayer<'_> {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (collections, tracker) = self.collections.get_heap_size_with_tracker(tracker);
        let (mappings, tracker) = self.mappings.get_heap_size_with_tracker(tracker);
        let (blocks, tracker) = self.blocks.get_heap_size_with_tracker(tracker);
        let (sources, tracker) = self.sources.get_heap_size_with_tracker(tracker);
        (
            collections
                .saturating_add(mappings)
                .saturating_add(blocks)
                .saturating_add(sources),
            tracker,
        )
    }
}
