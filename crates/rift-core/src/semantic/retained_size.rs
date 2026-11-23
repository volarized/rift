use super::{
    Contribution, ContributionEdge, ContributionKey, ContributionOrigin, ContributionReference,
    DeclarationBinding, EquivalenceEvidence, PortableSymbolFacts, SemanticReference, SymbolRecord,
};
use get_size2::{GetSize, GetSizeTracker};

macro_rules! heap_fields {
    ($kind:ty; $($field:ident),+ $(,)?) => {
        impl GetSize for $kind {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

            fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, mut tracker: T) -> (usize, T) {
                let mut bytes = 0_usize;
                $(
                    let (field, next) = self.$field.get_heap_size_with_tracker(tracker);
                    bytes = bytes.saturating_add(field);
                    tracker = next;
                )+
                (bytes, tracker)
            }
        }
    };
}

heap_fields!(ContributionReference; provider, symbol);
heap_fields!(ContributionKey; reference);
heap_fields!(DeclarationBinding; unit);
heap_fields!(ContributionOrigin; location);
heap_fields!(SemanticReference; source, targets);
heap_fields!(ContributionEdge; target, occurrence);
heap_fields!(SymbolRecord; identity, contributions);
impl GetSize for Contribution {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, mut tracker: T) -> (usize, T) {
        let mut bytes = self.namespaced_material_bytes;
        let (field, next) = self.key.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = self.facts.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = self.origin.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = self.source.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = self.identity_anchor.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = self.equivalence.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = self.references.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = self.edges.get_heap_size_with_tracker(tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        (bytes, tracker)
    }
}

impl GetSize for PortableSymbolFacts {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, mut tracker: T) -> (usize, T) {
        let mut bytes = 4 * std::mem::size_of::<usize>();
        macro_rules! field {
            ($field:ident) => {
                let (field, next) = self.$field.get_heap_size_with_tracker(tracker);
                bytes = bytes.saturating_add(field);
                tracker = next;
            };
        }
        field!(language);
        field!(kind);
        field!(container);
        let (field, next) =
            rift_protocol::retained_arc_material(&self.material, tracker, |material, tracker| {
                let (name, tracker) = material.name.get_heap_size_with_tracker(tracker);
                let (qualified, tracker) =
                    material.qualified_name.get_heap_size_with_tracker(tracker);
                let (kinds, tracker) = material.kinds.get_heap_size_with_tracker(tracker);
                let (modifiers, tracker) = material.modifiers.get_heap_size_with_tracker(tracker);
                let (visibility, tracker) = material.visibility.get_heap_size_with_tracker(tracker);
                let (types, tracker) =
                    rift_protocol::retained_vector_material(&material.types, tracker);
                let total = [name, qualified, kinds, modifiers, visibility, types]
                    .into_iter()
                    .fold(0usize, usize::saturating_add);
                (total, tracker)
            });
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = rift_protocol::retained_arc_slice_material(&self.signatures, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) =
            rift_protocol::retained_arc_slice_material(&self.documentation, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        (bytes, tracker)
    }
}

impl GetSize for EquivalenceEvidence {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Declaration(binding) => binding.get_heap_size_with_tracker(tracker),
            Self::Explicit(reference) | Self::Candidate(reference) => {
                reference.get_heap_size_with_tracker(tracker)
            }
        }
    }
}
