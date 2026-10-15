use super::*;
use get_size2::{GetSize, GetSizeTracker};

impl GetSize for DocumentIdentity {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        self.0.get_heap_size_with_tracker(tracker)
    }
}

impl GetSize for DocumentLocation {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Project(path) => path.get_heap_size_with_tracker(tracker),
            Self::Unit(unit) => unit.get_heap_size_with_tracker(tracker),
        }
    }
}

impl GetSize for DocumentFields {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (name, tracker) = self.name.get_heap_size_with_tracker(tracker);
        let (qualified, tracker) = self.qualified_name.get_heap_size_with_tracker(tracker);
        let (terms, tracker) = self.identifier_terms.get_heap_size_with_tracker(tracker);
        let (signature, tracker) = self.signature.get_heap_size_with_tracker(tracker);
        let (documentation, tracker) = self.documentation.get_heap_size_with_tracker(tracker);
        let (content, tracker) = self.file_content.get_heap_size_with_tracker(tracker);
        (
            name.saturating_add(qualified)
                .saturating_add(terms)
                .saturating_add(signature)
                .saturating_add(documentation)
                .saturating_add(content),
            tracker,
        )
    }
}

impl GetSize for IndexDocument {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (identity, tracker) = self.identity.get_heap_size_with_tracker(tracker);
        let (location, tracker) = self.location.get_heap_size_with_tracker(tracker);
        let (language, tracker) = self.language.get_heap_size_with_tracker(tracker);
        let (package, tracker) = self.package.get_heap_size_with_tracker(tracker);
        let (digest, tracker) = self.digest.get_heap_size_with_tracker(tracker);
        let (fields, tracker) = self.fields.get_heap_size_with_tracker(tracker);
        (
            identity
                .saturating_add(location)
                .saturating_add(language)
                .saturating_add(package)
                .saturating_add(digest)
                .saturating_add(fields),
            tracker,
        )
    }
}
