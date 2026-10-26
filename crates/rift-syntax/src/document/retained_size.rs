use get_size2::{GetSize, GetSizeTracker};

use super::{
    ByteRange, PythonOverload, RustModulePath, SyntaxExportBinding, SyntaxFacts, SyntaxNode,
    SyntaxSymbol,
};

impl GetSize for ByteRange {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }
}
// Grammar kind names are static; every other node field is stored in the row.
impl GetSize for SyntaxNode {}

impl GetSize for PythonOverload {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }
}

impl GetSize for RustModulePath {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Literal { path, .. } => path.get_heap_size_with_tracker(tracker),
            Self::Absent | Self::Unknown => (0, tracker),
        }
    }
}

impl GetSize for SyntaxExportBinding {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        self.container.get_heap_size_with_tracker(tracker)
    }
}

impl GetSize for SyntaxSymbol {
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
        let (container, tracker) = self.container.get_heap_size_with_tracker(tracker);
        let (facets, tracker) = self.facets.get_heap_size_with_tracker(tracker);
        let (visibility, tracker) = self.visibility.get_heap_size_with_tracker(tracker);
        let (signatures, tracker) =
            rift_protocol::retained_arc_slice_material(&self.signatures, tracker);
        let (documentation, tracker) =
            rift_protocol::retained_arc_slice_material(&self.documentation, tracker);
        let (ranges, tracker) = self
            .documentation_ranges
            .get_heap_size_with_tracker(tracker);
        let (module, tracker) = self.module_path.get_heap_size_with_tracker(tracker);
        // Both retained Arc slices hold strong and weak counts. Repeated references
        // can overcharge these headers, while their data is deduplicated by tracker.
        let bytes = [
            name,
            qualified,
            container,
            facets,
            visibility,
            signatures,
            documentation,
            ranges,
            module,
            4 * std::mem::size_of::<usize>(),
        ]
        .into_iter()
        .fold(0usize, usize::saturating_add);
        (bytes, tracker)
    }
}

impl GetSize for SyntaxFacts {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (language, tracker) = self.language.get_heap_size_with_tracker(tracker);
        let (nodes, tracker) = match &self.nodes {
            Some(nodes) => rift_protocol::retained_arc_slice_material(nodes, tracker),
            None => (0, tracker),
        };
        let (symbols, tracker) = rift_protocol::retained_vector_material(&self.symbols, tracker);
        let (retained, tracker) =
            rift_protocol::retained_vector_material(&self.left_out_symbols, tracker);
        let (exports, tracker) = self.export_bindings.get_heap_size_with_tracker(tracker);
        let macros = self.macro_invocations.as_ref().map_or(0, |rows| {
            rows.iter().fold(
                rows.capacity()
                    .saturating_mul(std::mem::size_of::<super::MacroInvocation>()),
                |charge, (_, modules, direct)| {
                    charge
                        .saturating_add(
                            modules
                                .capacity()
                                .saturating_mul(std::mem::size_of::<super::ByteRange>()),
                        )
                        .saturating_add(direct.as_ref().map_or(0, |ranges| {
                            ranges
                                .capacity()
                                .saturating_mul(std::mem::size_of::<super::ByteRange>())
                        }))
                },
            )
        });
        let scope = self.scope.as_ref().map_or(0, |rows| {
            rows.iter().fold(
                rows.capacity()
                    .saturating_mul(std::mem::size_of::<super::Scope>()),
                |charge, (_, children)| {
                    charge.saturating_add(children.as_ref().map_or(0, |ranges| {
                        ranges
                            .capacity()
                            .saturating_mul(std::mem::size_of::<super::ByteRange>())
                    }))
                },
            )
        });
        let (markdown, tracker) = self.markdown_facts.get_heap_size_with_tracker(tracker);
        (
            language
                .saturating_add(nodes)
                .saturating_add(symbols)
                .saturating_add(retained)
                .saturating_add(exports)
                .saturating_add(macros)
                .saturating_add(scope)
                .saturating_add(markdown),
            tracker,
        )
    }
}
