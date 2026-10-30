use get_size2::{GetSize, GetSizeTracker};

impl GetSize for crate::identity::SymbolOwner {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Local => (0, tracker),
            Self::NamedLocal { name } | Self::ProfileLocal { name } => {
                name.get_heap_size_with_tracker(tracker)
            }
            Self::NamedProfileLocal { project, name } => {
                let (project, tracker) = project.get_heap_size_with_tracker(tracker);
                let (name, tracker) = name.get_heap_size_with_tracker(tracker);
                (project.saturating_add(name), tracker)
            }
            Self::Package {
                manager,
                registry,
                name,
                version,
            } => {
                let (manager, tracker) = manager.get_heap_size_with_tracker(tracker);
                let (registry, tracker) = registry.get_heap_size_with_tracker(tracker);
                let (name, tracker) = name.get_heap_size_with_tracker(tracker);
                let (version, tracker) = version.get_heap_size_with_tracker(tracker);
                (
                    manager
                        .saturating_add(registry)
                        .saturating_add(name)
                        .saturating_add(version),
                    tracker,
                )
            }
            Self::Runtime { runtime, version } => {
                let (runtime, tracker) = runtime.get_heap_size_with_tracker(tracker);
                let (version, tracker) = version.get_heap_size_with_tracker(tracker);
                (runtime.saturating_add(version), tracker)
            }
        }
    }
}

impl GetSize for crate::read::SourceLocation {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Project { package } => package.get_heap_size_with_tracker(tracker),
            Self::Dependency { package } => package.get_heap_size_with_tracker(tracker),
            Self::Stdlib { runtime } => runtime.get_heap_size_with_tracker(tracker),
            Self::External {} => (0, tracker),
        }
    }
}

impl GetSize for crate::read::SourceUnitId {
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

use crate::read::{
    Documentation, ExactKind, ExtensionKey, Extensions, Language, PackageIdentity, Parameter,
    RuntimeIdentity, Signature, SignatureLink, Symbol, SymbolFacet, SymbolId, SymbolOrigin,
    TypeBinding, TypeExpression,
};

macro_rules! heap_field {
    ($value:expr, $tracker:expr) => {
        $value.get_heap_size_with_tracker($tracker)
    };
    ($value:expr, $tracker:expr, vector) => {
        retained_vector_material($value, $tracker)
    };
}

macro_rules! heap_fields {
    ($type:ty, $($field:tt $(=> $measure:ident)?),+ $(,)?) => {
        impl GetSize for $type {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

            fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
                let mut total = 0usize;
                $(
                    let (bytes, tracker) = heap_field!(&self.$field, tracker $(, $measure)?);
                    total = total.saturating_add(bytes);
                )+
                (total, tracker)
            }
        }
    };
}

heap_fields!(crate::read::Digest, 0);
heap_fields!(crate::read::ProjectPath, 0);
heap_fields!(crate::documentation::DocumentationDigest, 0);
heap_fields!(crate::documentation::NotebookCell, identity);
heap_fields!(
    crate::documentation::DocumentationContentIdentity,
    source,
    cell
);
heap_fields!(
    crate::documentation::DocumentationLicense,
    expression,
    files
);
heap_fields!(crate::documentation::DocumentationLicenseFile, path, digest);
heap_fields!(
    crate::documentation::DocumentationSource,
    identity,
    revision,
    content_digest,
    origin,
    media_type,
    language,
    physical_ranges,
    license
);
heap_fields!(crate::documentation::DocumentationHeading, name);
heap_fields!(crate::documentation::DocumentationChunk, identity);
heap_fields!(
    crate::documentation::DocumentationBlock,
    identity,
    source,
    content_digest,
    heading_path,
    language,
    chunks,
    symbol
);
heap_fields!(
    crate::documentation::DocumentationLink,
    block,
    authored,
    resolution
);
heap_fields!(
    crate::documentation::DocumentationReference,
    identity,
    block,
    target,
    authored
);
heap_fields!(
    crate::documentation::DocumentationReferenceCandidate,
    block,
    authored,
    language
);
heap_fields!(crate::documentation::DocumentationWarning, source);
heap_fields!(
    crate::documentation::DocumentationIndex,
    documentation_revision,
    selection_digest,
    sources,
    blocks,
    links,
    references,
    unresolved_references,
    warnings
);

impl GetSize for crate::read::TextRange {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }
}
heap_fields!(crate::read::FileId, 0);
heap_fields!(
    crate::symbol_read::SymbolDeclaration,
    origin,
    path,
    unit,
    source,
    signature_indices,
    type_indices,
    documentation_indices
);

impl GetSize for crate::documentation::NotebookCellIdentity {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Authored { id } => id.get_heap_size_with_tracker(tracker),
            Self::Indexed { .. } => (0, tracker),
        }
    }
}

impl GetSize for crate::documentation::DocumentationSourceIdentity {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Project { path } => path.get_heap_size_with_tracker(tracker),
            Self::Package { unit } => unit.get_heap_size_with_tracker(tracker),
        }
    }
}

impl GetSize for crate::documentation::DocumentationTarget {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Block { identity } => identity.get_heap_size_with_tracker(tracker),
            Self::Source { source, .. } => source.get_heap_size_with_tracker(tracker),
            Self::Symbol { symbol } => symbol.get_heap_size_with_tracker(tracker),
        }
    }
}

impl GetSize for crate::documentation::DocumentationLinkResolution {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        match self {
            Self::Resolved { target } => target.get_heap_size_with_tracker(tracker),
            Self::Unresolved { .. } => (0, tracker),
        }
    }
}

heap_fields!(Language, name, dialect);
heap_fields!(Documentation, text);
heap_fields!(ExactKind, 0);
heap_fields!(SymbolId, 0);
heap_fields!(ExtensionKey, 0);
heap_fields!(PackageIdentity, manager, registry, name, version);
heap_fields!(RuntimeIdentity, runtime, version);
heap_fields!(SymbolOrigin, package, runtime);
heap_fields!(SignatureLink, symbol);
heap_fields!(TypeBinding, r#type);
heap_fields!(TypeExpression, language, source, resolved, extensions);
heap_fields!(Parameter, name, types => vector, default, extensions);
heap_fields!(
    Signature,
    display,
    links => vector,
    language,
    receiver,
    parameters => vector,
    returns => vector,
    type_parameters => vector,
    throws => vector,
    effects => vector,
    extensions,
);
heap_fields!(
    Symbol,
    id,
    language,
    name,
    kind,
    facets => vector,
    origin,
    container,
    modifiers => vector,
    visibility,
    types => vector,
    signatures => vector,
    documentation => vector,
    extensions,
);

impl GetSize for SymbolFacet {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }
}

impl GetSize for Extensions {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let mut total =
            btree_material_bytes::<ExtensionKey, crate::read::ExtensionValue>(self.0.len());
        let mut tracker = tracker;
        for (key, value) in &self.0 {
            let (bytes, next) = key.get_heap_size_with_tracker(tracker);
            tracker = next;
            total = total
                .saturating_add(bytes)
                .saturating_add(json_heap(&value.data));
        }
        (total, tracker)
    }
}

fn json_heap(value: &serde_json::Value) -> usize {
    let mut values = vec![value];
    let mut total = 0usize;
    while let Some(value) = values.pop() {
        match value {
            serde_json::Value::String(text) => total = total.saturating_add(text.capacity()),
            serde_json::Value::Array(array) => {
                total = total.saturating_add(
                    array
                        .capacity()
                        .saturating_mul(std::mem::size_of::<serde_json::Value>()),
                );
                values.extend(array);
            }
            // Map does not expose its private backing allocation's capacity. Admission
            // refuses this unmeasured value until its owning construction supplies a bound.
            serde_json::Value::Object(_) => return usize::MAX,
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }
    total
}

/// Counts an owned value without overflowing an unmeasured heap refusal.
#[must_use]
pub fn retained_value_material<V: GetSize, T: GetSizeTracker>(value: &V, tracker: T) -> (usize, T) {
    let (heap, tracker) = value.get_heap_size_with_tracker(tracker);
    (std::mem::size_of::<V>().saturating_add(heap), tracker)
}

/// Counts an allocation and its elements' owned heap material.
#[must_use]
pub fn retained_sequence_material<'a, V: 'a, T: GetSizeTracker>(
    values: impl IntoIterator<Item = &'a V>,
    allocation: usize,
    mut tracker: T,
    mut measure: impl FnMut(&V, T) -> (usize, T),
) -> (usize, T) {
    let mut bytes = allocation;
    for value in values {
        let (part, next) = measure(value, tracker);
        bytes = bytes.saturating_add(part);
        tracker = next;
    }
    (bytes, tracker)
}

/// Counts vector capacity and each element's retained heap material.
#[must_use]
pub fn retained_vector_material<V: GetSize, T: GetSizeTracker>(
    value: &Vec<V>,
    tracker: T,
) -> (usize, T) {
    retained_sequence_material(
        value,
        value.capacity().saturating_mul(std::mem::size_of::<V>()),
        tracker,
        GetSize::get_heap_size_with_tracker,
    )
}

/// Counts one shared slice allocation and each element's retained heap material.
#[must_use]
pub fn retained_arc_slice_material<V: GetSize, T: GetSizeTracker>(
    value: &std::sync::Arc<[V]>,
    tracker: T,
) -> (usize, T) {
    retained_arc_material(value, tracker, |values, tracker| {
        retained_sequence_material(values, 0, tracker, GetSize::get_heap_size_with_tracker)
    })
}

/// Counts one shared allocation, including its value and reference counts.
#[must_use]
pub fn retained_arc_material<V: ?Sized, T: GetSizeTracker>(
    value: &std::sync::Arc<V>,
    mut tracker: T,
    measure: impl FnOnce(&V, T) -> (usize, T),
) -> (usize, T) {
    if !tracker.track(std::sync::Arc::as_ptr(value).cast::<()>()) {
        return (0, tracker);
    }
    let (heap, tracker) = measure(value.as_ref(), tracker);
    let bytes = std::mem::size_of_val(value.as_ref())
        .saturating_add(2 * std::mem::size_of::<usize>())
        .saturating_add(heap);
    (bytes, tracker)
}

/// Counts retained map nodes and owned key and value heap material.
#[must_use]
pub fn retained_map_material<K: GetSize + Ord, V, T: GetSizeTracker>(
    values: &std::collections::BTreeMap<K, V>,
    mut tracker: T,
    mut measure: impl FnMut(&V, T) -> (usize, T),
) -> (usize, T) {
    let mut bytes = btree_material_bytes::<K, V>(values.len());
    for (key, value) in values {
        let (key, next) = key.get_heap_size_with_tracker(tracker);
        let (value, next) = measure(value, next);
        bytes = bytes.saturating_add(key).saturating_add(value);
        tracker = next;
    }
    (bytes, tracker)
}

// Rust 1.98 alloc::collections::btree::node: 11 key/value slots, a parent
// pointer, two u16 fields, and at most 12 child pointers per allocated node.
// Each field is rounded to the largest field alignment before summation.
// Charging one full internal node per entry plus one root overcharges empty
// slots and leaf nodes; it does not claim allocator or resident-memory size.
/// Conservative material occupied by the nodes of a retained `BTreeMap` or `BTreeSet`.
///
/// This bound uses the pinned Rust node layout and includes unused node slots.
/// Every non-root node retains at least five entries; one root is counted separately.
/// Overflow returns `usize::MAX`, so a finite admission budget refuses it.
#[must_use]
pub fn btree_material_bytes<K, V>(len: usize) -> usize {
    let alignment = std::mem::align_of::<K>()
        .max(std::mem::align_of::<V>())
        .max(std::mem::align_of::<usize>());
    let fields = [
        std::mem::size_of::<usize>(),
        std::mem::size_of::<u16>(),
        std::mem::size_of::<u16>(),
        std::mem::size_of::<K>().saturating_mul(11),
        std::mem::size_of::<V>().saturating_mul(11),
        std::mem::size_of::<usize>().saturating_mul(12),
    ];
    let node = fields.into_iter().try_fold(0usize, |total, field| {
        field
            .checked_add(alignment - 1)
            .map(|padded| padded / alignment * alignment)
            .and_then(|padded| total.checked_add(padded))
    });
    (len / 5)
        .checked_add(1)
        .and_then(|nodes| node.and_then(|node| nodes.checked_mul(node)))
        .unwrap_or(usize::MAX)
}

heap_fields!(crate::read::SourceUnitSpan, unit);
heap_fields!(crate::read::Relationship, from, kind, facets => vector, to, occurrence, extensions);
heap_fields!(crate::search::GraphHop, relationship);

impl GetSize for crate::read::RelationshipFacet {
    fn get_heap_size_with_tracker<T: GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        (0, tracker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_owner_material_counts_name_capacity() {
        for profile in [false, true] {
            let mut name = String::with_capacity(512);
            name.push_str("docs");
            let capacity = name.capacity();
            let owner = if profile {
                crate::identity::SymbolOwner::ProfileLocal { name }
            } else {
                crate::identity::SymbolOwner::NamedLocal { name }
            };
            assert_eq!(owner.get_heap_size(), capacity);
            assert_eq!(owner.get_size(), std::mem::size_of_val(&owner) + capacity);
            assert_eq!(
                owner
                    .get_size_with_tracker(get_size2::StandardTracker::new())
                    .0,
                std::mem::size_of_val(&owner) + capacity
            );
        }
    }

    #[test]
    fn extra_profile_owner_material_counts_both_name_capacities() {
        let mut project = String::with_capacity(256);
        project.push_str("cloud");
        let mut name = String::with_capacity(512);
        name.push_str("docs");
        let capacity = project.capacity() + name.capacity();
        let owner = crate::identity::SymbolOwner::NamedProfileLocal { project, name };
        assert_eq!(owner.get_heap_size(), capacity);
        assert_eq!(owner.get_size(), std::mem::size_of_val(&owner) + capacity);
    }

    #[test]
    fn retained_relationship_material_counts_capacity_and_refuses_unmeasured_facts() {
        let mut kind = String::with_capacity(256);
        kind.push_str("calls");
        let mut facets = Vec::with_capacity(32);
        facets.push(crate::read::RelationshipFacet::Calls);
        let edge = crate::read::Relationship {
            from: SymbolId("rift://symbol/local/rust/app/caller".to_owned()),
            kind: ExactKind(kind),
            facets,
            to: SymbolId("rift://symbol/local/rust/app/callee".to_owned()),
            occurrence: Some(crate::read::SourceUnitSpan {
                unit: crate::read::SourceUnitId("rift://source/project/src/lib.rs".to_owned()),
                range: crate::read::TextRange { start: 0, end: 12 },
            }),
            derivation: crate::read::RelationshipDerivation::Resolution,
            confidence: None,
            extensions: Extensions::default(),
        };
        let minimum = edge.kind.0.capacity()
            + edge.facets.capacity() * std::mem::size_of::<crate::read::RelationshipFacet>()
            + edge.from.0.capacity()
            + edge.to.0.capacity();
        let hop = crate::search::GraphHop {
            relationship: edge,
            direction: crate::search::HopDirection::Outgoing,
        };
        let measured = hop
            .get_size_with_tracker(get_size2::StandardTracker::new())
            .0;
        assert!(measured >= std::mem::size_of_val(&hop) + minimum);
        assert_ne!(measured, usize::MAX);
        let mut unmeasured = hop;
        unmeasured.relationship.extensions = Extensions(std::collections::BTreeMap::from([(
            ExtensionKey("example.org.fact".to_owned()),
            crate::read::ExtensionValue {
                version: 1,
                data: serde_json::json!({"held": true}),
            },
        )]));
        assert_eq!(unmeasured.get_size(), usize::MAX);
        let values = vec![unmeasured];
        assert_eq!(
            retained_vector_material(&values, get_size2::StandardTracker::new()).0,
            usize::MAX
        );
    }

    #[test]
    fn retained_fact_material_counts_capacity_and_shared_arcs_once() {
        let mut text = String::with_capacity(512);
        text.push_str("documentation");
        let capacity = text.capacity();
        let documentation = std::sync::Arc::new(Documentation {
            format: crate::read::DocumentationFormat::Plain,
            text,
        });
        let one = retained_arc_material(
            &documentation,
            get_size2::StandardTracker::new(),
            GetSize::get_heap_size_with_tracker,
        )
        .0;
        let values = vec![std::sync::Arc::clone(&documentation), documentation];
        let allocation = values.capacity() * std::mem::size_of::<std::sync::Arc<Documentation>>();
        let both = retained_sequence_material(
            &values,
            allocation,
            get_size2::StandardTracker::new(),
            |value, tracker| {
                retained_arc_material(value, tracker, GetSize::get_heap_size_with_tracker)
            },
        )
        .0;
        assert!(one >= 512);
        assert_eq!(
            one,
            capacity + std::mem::size_of::<Documentation>() + 2 * std::mem::size_of::<usize>()
        );
        assert_eq!(both, one + allocation);
    }

    #[test]
    fn tree_material_refuses_overflow_and_counts_empty_root() {
        let empty = btree_material_bytes::<String, String>(0);
        assert!(empty > 0);
        for len in [1, 4] {
            assert_eq!(btree_material_bytes::<String, String>(len), empty);
        }
        for len in [5, 9] {
            assert_eq!(btree_material_bytes::<String, String>(len), empty * 2);
        }
        for len in [10, 11, 12, 14] {
            assert_eq!(btree_material_bytes::<String, String>(len), empty * 3);
        }
        assert_eq!(btree_material_bytes::<String, String>(15), empty * 4);
        assert_eq!(
            btree_material_bytes::<String, String>(usize::MAX),
            usize::MAX
        );
    }

    #[test]
    fn retained_maps_count_distinct_nodes_and_shared_value_material_once() {
        let value = std::sync::Arc::new(vec![0u8; 4096]);
        let first: std::collections::BTreeMap<u32, _> = (0..12)
            .map(|key| (key, std::sync::Arc::clone(&value)))
            .collect();
        let second = first.clone();
        let measure = |value: &std::sync::Arc<Vec<u8>>, tracker| {
            retained_arc_material(value, tracker, GetSize::get_heap_size_with_tracker)
        };
        let (first_bytes, tracker) =
            retained_map_material(&first, get_size2::StandardTracker::new(), measure);
        let (second_bytes, _) = retained_map_material(&second, tracker, measure);
        let nodes = btree_material_bytes::<u32, std::sync::Arc<Vec<u8>>>(12);
        let value_bytes =
            std::mem::size_of::<Vec<u8>>() + 2 * std::mem::size_of::<usize>() + value.capacity();
        assert_eq!(first_bytes, nodes + value_bytes);
        assert_eq!(second_bytes, nodes);
        assert_ne!(first_bytes, usize::MAX);
    }

    #[test]
    fn unmeasured_extensions_refuse_nested_material_without_overflow() {
        let extensions = Extensions(std::collections::BTreeMap::from([(
            ExtensionKey("example.org.fact".to_owned()),
            crate::read::ExtensionValue {
                version: 1,
                data: serde_json::json!({"held": "value"}),
            },
        )]));
        let signature = Signature {
            display: "call(value)".to_owned(),
            links: Vec::new(),
            language: Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            receiver: None,
            parameters: vec![Parameter {
                name: Some("value".to_owned()),
                types: Vec::new(),
                optional: false,
                variadic: false,
                default: None,
                extensions: extensions.clone(),
            }],
            returns: Vec::new(),
            type_parameters: Vec::new(),
            throws: Vec::new(),
            effects: Vec::new(),
            extensions: Extensions::default(),
        };
        assert_eq!(extensions.get_size(), usize::MAX);
        assert_eq!(signature.get_size(), usize::MAX);
        let values = vec![signature.clone(), signature.clone()];
        assert_eq!(
            retained_vector_material(&values, get_size2::StandardTracker::new()).0,
            usize::MAX
        );
        let slice: std::sync::Arc<[Signature]> = std::sync::Arc::from(values);
        assert_eq!(
            retained_arc_slice_material(&slice, get_size2::StandardTracker::new()).0,
            usize::MAX
        );
        let boxed = vec![signature.clone()].into_boxed_slice();
        assert_eq!(
            retained_sequence_material(
                boxed.iter(),
                std::mem::size_of_val(boxed.as_ref()),
                get_size2::StandardTracker::new(),
                GetSize::get_heap_size_with_tracker,
            )
            .0,
            usize::MAX
        );
        let shared = std::sync::Arc::new(signature);
        let values = std::collections::BTreeMap::from([
            ("first".to_owned(), std::sync::Arc::clone(&shared)),
            ("second".to_owned(), shared),
        ]);
        assert_eq!(
            retained_map_material(
                &values,
                get_size2::StandardTracker::new(),
                |value, tracker| {
                    retained_arc_material(value, tracker, GetSize::get_heap_size_with_tracker)
                }
            )
            .0,
            usize::MAX
        );
    }

    #[test]
    fn json_material_refuses_hidden_object_capacity() {
        assert_eq!(json_heap(&serde_json::json!({"value": "held"})), usize::MAX);
        assert_eq!(
            json_heap(&serde_json::Value::String(String::with_capacity(128))),
            128
        );
    }

    #[test]
    fn retained_type_vectors_refuse_nested_unmeasured_values_without_overflow() {
        let binding = TypeBinding {
            role: crate::read::TypeBindingRole::Return,
            origin: crate::read::TypeBindingOrigin::Declared,
            r#type: TypeExpression {
                language: Language {
                    name: "rust".to_owned(),
                    dialect: None,
                },
                source: "Result".to_owned(),
                resolved: None,
                extensions: Extensions(std::collections::BTreeMap::from([(
                    ExtensionKey("example.org.fact".to_owned()),
                    crate::read::ExtensionValue {
                        version: 1,
                        data: serde_json::json!({"held": true}),
                    },
                )])),
            },
        };
        assert_eq!(binding.get_size(), usize::MAX);
        let nested = vec![vec![binding.clone()], vec![binding]];
        let (bytes, _) = retained_sequence_material(
            &nested,
            nested
                .capacity()
                .saturating_mul(std::mem::size_of::<Vec<TypeBinding>>()),
            get_size2::StandardTracker::new(),
            retained_vector_material,
        );
        assert_eq!(bytes, usize::MAX);
    }
}
