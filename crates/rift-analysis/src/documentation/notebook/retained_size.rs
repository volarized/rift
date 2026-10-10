use super::{NotebookCellContent, NotebookContent};
use get_size2::{GetSize, GetSizeTracker};

impl GetSize for NotebookCellContent {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (text, tracker) = self.text.get_heap_size_with_tracker(tracker);
        let (cell, tracker) = self.cell.get_heap_size_with_tracker(tracker);
        let (ranges, tracker) = self.physical_ranges.get_heap_size_with_tracker(tracker);
        let (language, tracker) = self.declared_language.get_heap_size_with_tracker(tracker);
        (
            text.saturating_add(cell)
                .saturating_add(ranges)
                .saturating_add(language),
            tracker,
        )
    }
}

impl GetSize for NotebookContent {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (cells, tracker) = self.cells.get_heap_size_with_tracker(tracker);
        let (warnings, tracker) = self.warnings.get_heap_size_with_tracker(tracker);
        (cells.saturating_add(warnings), tracker)
    }
}
