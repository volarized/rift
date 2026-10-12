use super::{
    BTreeSet, CachedBlock, DependencyKey, DependencyState, DocumentationDigest, FragmentKey,
    LanguageKey, NameState, ReferenceState, ResolutionCache,
};
use get_size2::{GetSize, GetSizeTracker};

impl GetSize for ResolutionCache {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (revision, tracker) = self.revision.get_heap_size_with_tracker(tracker);
        let (blocks, tracker) = self.blocks.get_heap_size_with_tracker(tracker);
        let (reverse, tracker) = self.reverse.get_heap_size_with_tracker(tracker);
        let (states, tracker) = self.states.get_heap_size_with_tracker(tracker);
        let mut bytes = revision
            .saturating_add(blocks)
            .saturating_add(reverse)
            .saturating_add(states);
        bytes = bytes.saturating_add(rift_protocol::btree_material_bytes::<
            DocumentationDigest,
            CachedBlock,
        >(self.blocks.len()));
        bytes = bytes.saturating_add(rift_protocol::btree_material_bytes::<
            DependencyKey,
            BTreeSet<DocumentationDigest>,
        >(self.reverse.len()));
        bytes = bytes.saturating_add(rift_protocol::btree_material_bytes::<
            DependencyKey,
            DependencyState,
        >(self.states.len()));
        for values in self.reverse.values() {
            bytes = bytes
                .saturating_add(
                    rift_protocol::btree_material_bytes::<DocumentationDigest, ()>(values.len()),
                );
        }
        (bytes, tracker)
    }
}

impl GetSize for CachedBlock {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (input, tracker) = self.input.get_heap_size_with_tracker(tracker);
        let (links, tracker) = self.resolvable_links.get_heap_size_with_tracker(tracker);
        let (unresolved, tracker) = self
            .fixed_unresolved_links
            .get_heap_size_with_tracker(tracker);
        let (references, tracker) = self.references.get_heap_size_with_tracker(tracker);
        let (candidates, tracker) = self.candidate_results.get_heap_size_with_tracker(tracker);
        (
            input
                .saturating_add(links)
                .saturating_add(unresolved)
                .saturating_add(references)
                .saturating_add(candidates),
            tracker,
        )
    }
}

impl GetSize for DependencyKey {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Target { source, fragment } => {
                let (source, tracker) = source.get_heap_size_with_tracker(tracker);
                let (fragment, tracker) = fragment.get_heap_size_with_tracker(tracker);
                (source.saturating_add(fragment), tracker)
            }
            Self::DirectSymbol(value) => value.get_heap_size_with_tracker(tracker),
            Self::Qualified { source, name } => {
                let (source, tracker) = source.get_heap_size_with_tracker(tracker);
                let (name, tracker) = name.get_heap_size_with_tracker(tracker);
                (source.saturating_add(name), tracker)
            }
            Self::Reference { authored, language } => {
                let (authored, tracker) = authored.get_heap_size_with_tracker(tracker);
                let (language, tracker) = language.get_heap_size_with_tracker(tracker);
                (authored.saturating_add(language), tracker)
            }
        }
    }
}

impl GetSize for FragmentKey {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Name(value) => value.get_heap_size_with_tracker(tracker),
            Self::WholeSource | Self::Invalid => (0, tracker),
        }
    }
}

impl GetSize for LanguageKey {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (name, tracker) = self.name.get_heap_size_with_tracker(tracker);
        let (dialect, tracker) = self.dialect.get_heap_size_with_tracker(tracker);
        (name.saturating_add(dialect), tracker)
    }
}

impl GetSize for DependencyState {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Target(_) => (0, tracker),
            Self::DirectSymbol(value) => value.get_heap_size_with_tracker(tracker),
            Self::Qualified(value) => value.get_heap_size_with_tracker(tracker),
            Self::Reference(value) => value.get_heap_size_with_tracker(tracker),
        }
    }
}

impl GetSize for NameState {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Symbol(value) => value.get_heap_size_with_tracker(tracker),
            Self::Missing | Self::Ambiguous => (0, tracker),
        }
    }
}

impl GetSize for ReferenceState {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Match(value, _) => value.get_heap_size_with_tracker(tracker),
            Self::Missing | Self::Ambiguous => (0, tracker),
        }
    }
}
