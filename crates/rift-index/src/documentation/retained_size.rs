use super::{HeldNotebook, NotebookOutcome};
use get_size2::{GetSize, GetSizeTracker};

impl GetSize for NotebookOutcome {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Decoded(content) => content.get_heap_size_with_tracker(tracker),
            Self::Omitted(_) => (0, tracker),
        }
    }
}

impl GetSize for HeldNotebook {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (bytes, tracker) = self.outcome.get_heap_size_with_tracker(tracker);
        (
            bytes.saturating_add(2 * std::mem::size_of::<usize>()),
            tracker,
        )
    }
}
