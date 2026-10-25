//! Retained provider metadata measured through the existing material owner.

use get_size2::{GetSize, GetSizeTracker};

use super::{
    QueryProviderContext, QueryProviderDeclaration, QueryProviderEdge, QueryProviderEndpoint,
    QueryProviderInput, QueryProviderRecord, QueryProviderReference, QueryProviderRelationship,
    QueryProviderSnapshot, QueryProviderTarget,
};

macro_rules! heap_fields {
    ($kind:ty; fields [$($field:ident),*]; vectors [$($vector:ident),*]) => {
        impl GetSize for $kind {
            fn get_size(&self) -> usize {
                std::mem::size_of::<Self>().saturating_add(self.get_heap_size())
            }

            fn get_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
                let (heap, tracker) = self.get_heap_size_with_tracker(tracker);
                (std::mem::size_of::<Self>().saturating_add(heap), tracker)
            }

            fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, mut tracker: T) -> (usize, T) {
                let mut bytes = 0usize;
                $(
                    let (part, next) = self.$field.get_heap_size_with_tracker(tracker);
                    bytes = bytes.saturating_add(part);
                    tracker = next;
                )*
                $(
                    let (part, next) = rift_protocol::retained_vector_material(&self.$vector, tracker);
                    bytes = bytes.saturating_add(part);
                    tracker = next;
                )*
                (bytes, tracker)
            }
        }
    };
}

heap_fields!(QueryProviderContext; fields [selection, root, owner, source_digest]; vectors [inputs]);
heap_fields!(QueryProviderInput; fields [uri, digest]; vectors []);
heap_fields!(QueryProviderSnapshot; fields [context]; vectors [records, source_indices, references, relationships]);
heap_fields!(QueryProviderRecord; fields [identity, container]; vectors [contributions, declarations]);
heap_fields!(QueryProviderDeclaration; fields [contribution, binding]; vectors []);
heap_fields!(QueryProviderReference; fields [source, binding]; vectors [targets]);
heap_fields!(QueryProviderRelationship; fields [source, target, occurrence]; vectors []);
heap_fields!(QueryProviderEndpoint; fields [target]; vectors []);
heap_fields!(QueryProviderEdge; fields [from, to, occurrence]; vectors []);

impl GetSize for QueryProviderTarget {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.get_heap_size())
    }

    fn get_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = self.get_heap_size_with_tracker(tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (target, tracker) = self.target.get_heap_size_with_tracker(tracker);
        let Some(context) = &self.context else {
            return (target, tracker);
        };
        let (context, tracker) = rift_protocol::retained_arc_material(
            context,
            tracker,
            GetSize::get_heap_size_with_tracker,
        );
        (target.saturating_add(context), tracker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_record_retains_spare_association_capacity() {
        let record = QueryProviderRecord {
            symbol_index: 0,
            identity: None,
            contributions: Vec::with_capacity(32),
            declarations: Vec::with_capacity(16),
            container: None,
        };
        let expected = record.contributions.capacity()
            * std::mem::size_of::<rift_core::ContributionKey>()
            + record.declarations.capacity() * std::mem::size_of::<QueryProviderDeclaration>();
        assert_eq!(record.get_heap_size(), expected);
        assert_eq!(
            record.get_size(),
            std::mem::size_of::<QueryProviderRecord>() + expected
        );
    }
}
