use get_size2::{GetSize, GetSizeTracker};

use super::{
    MarkdownBlockFact, MarkdownFacts, MarkdownHeadingFact, MarkdownLinkFact,
    MarkdownReferenceCandidate,
};

impl GetSize for MarkdownHeadingFact {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }
}
impl GetSize for MarkdownLinkFact {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }
}
impl GetSize for MarkdownReferenceCandidate {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }
}

impl GetSize for MarkdownBlockFact {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        self.code_language.get_heap_size_with_tracker(tracker)
    }
}

impl GetSize for MarkdownFacts {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (blocks, tracker) = self.blocks.get_heap_size_with_tracker(tracker);
        let (headings, tracker) = self.headings.get_heap_size_with_tracker(tracker);
        let (links, tracker) = self.links.get_heap_size_with_tracker(tracker);
        let (references, tracker) = self
            .reference_candidates
            .get_heap_size_with_tracker(tracker);
        let (errors, tracker) = self.error_ranges.get_heap_size_with_tracker(tracker);
        let (omitted, tracker) = self.omitted_ranges.get_heap_size_with_tracker(tracker);
        (
            blocks
                .saturating_add(headings)
                .saturating_add(links)
                .saturating_add(references)
                .saturating_add(errors)
                .saturating_add(omitted),
            tracker,
        )
    }
}
