//! Bidirectional symbol reference adjacency, derived once from one [`NormalizedGraph`].
//!
//! [`EdgeStore::build`] walks [`NormalizedGraph::references`] once and emits one
//! [`Edge`] per reference target a provider resolved to an established symbol
//! (`NormalizedTarget::Symbol`); a target the graph could not resolve
//! (`NormalizedTarget::Contribution`) contributes no edge. `NormalizedGraph::references`
//! carries resolution. Syntax headers retain syntax derivation, and normalized edges
//! retain the derivation their provider supplied.
//!
//! An edge's `from` is the enclosing definition: the definition record whose contribution
//! range is the smallest range containing the reference occurrence in the same source unit.
//! One [`EnclosingDefinitions`] per unit holds those ranges, built once from every
//! `Contribution::source` binding a record's contributions carry, and resolves each
//! occurrence against it. A reference outside every declaration uses the source module's
//! canonical identity, derived from the common parent of the unit's declarations.
//!
//! An edge's `kind` is [`role_kind`]'s mapping of the reference's [`ReferenceRole`].
//!
//! An edge's `occurrence` is the reference's own [`DeclarationBinding`]: unit and byte range.
//!
//! Building stops at [`EDGES_MAX`] edges and [`EdgeStore::dropped_edges`] counts the rest. Static
//! resolution ends on its work budgets. [`EdgeStore::is_complete`] reports both.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use rift_core::{DeclarationBinding, LoopBudget, ReferenceRole, SourceUnitId, SymbolId};
use rift_protocol::read::EdgeKind;
use rift_provider::{NormalizedGraph, NormalizedReference, NormalizedTarget};

use crate::EnclosingDefinitions;
use crate::static_resolution::{ExternalOccurrence, StaticOccurrence, StaticResolution};

/// Most edges one [`EdgeStore`] holds across both directions combined.
///
/// The publication set a build normalizes is already bounded by
/// [`PublicationLimits`](rift_provider::PublicationLimits), so this cap sits above what any
/// accepted publication can carry. It bounds the store's own construction work and memory
/// against a graph built outside those limits.
pub const EDGES_MAX: usize = 10_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileModuleReference {
    None,
    Binding,
    Specifier,
    CommonJsExportImport,
    IncompleteLocalExport,
    ImportedExportSelector,
    InlineImportType,
    CommonJsArgumentImport,
    InlineImportModule,
}

/// One resolved reference edge: the enclosing definition a reference occurred in, the
/// established symbol it resolved to, and the reference's portable category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    from: SymbolId,
    to: SymbolId,
    kind: EdgeKind,
    occurrence: DeclarationBinding,
    static_callee: Option<rift_syntax::ByteRange>,
    derivation: rift_protocol::read::EdgeDerivation,
    from_graph: bool,
    file_module_reference: FileModuleReference,
}

impl Edge {
    /// Whether the normalized graph already carries this edge.
    pub(crate) const fn is_graph_edge(&self) -> bool {
        self.from_graph
    }

    /// Whether this reference waits for checked ECMAScript package projection.
    pub(crate) const fn is_file_module_reference(&self) -> bool {
        !matches!(self.file_module_reference, FileModuleReference::None)
    }

    /// A checked import at the written module specifier.
    pub(crate) const fn is_written_module_specifier(&self) -> bool {
        matches!(self.file_module_reference, FileModuleReference::Specifier)
    }

    pub(crate) const fn is_deferred_commonjs_export_import(&self) -> bool {
        matches!(
            self.file_module_reference,
            FileModuleReference::CommonJsExportImport
        )
    }

    /// A local selector checked independently in an incomplete declaration file.
    pub(crate) const fn is_incomplete_local_export_reference(&self) -> bool {
        matches!(
            self.file_module_reference,
            FileModuleReference::IncompleteLocalExport
        )
    }

    /// A reference at a checked export selector naming an imported declaration.
    pub(crate) const fn is_imported_export_selector(&self) -> bool {
        matches!(
            self.file_module_reference,
            FileModuleReference::ImportedExportSelector
        )
    }

    /// A written qualifier in a checked relative inline TypeScript import type.
    pub(crate) const fn is_inline_import_type_reference(&self) -> bool {
        matches!(
            self.file_module_reference,
            FileModuleReference::InlineImportType
        )
    }

    /// The module specifier in a checked relative inline TypeScript import type.
    pub(crate) const fn is_inline_import_module(&self) -> bool {
        matches!(
            self.file_module_reference,
            FileModuleReference::InlineImportModule
        )
    }

    /// A checked literal load used as a direct call argument.
    pub(crate) const fn is_commonjs_argument_import(&self) -> bool {
        matches!(
            self.file_module_reference,
            FileModuleReference::CommonJsArgumentImport
        )
    }

    /// Projects this resolved reference with its physical occurrence.
    #[must_use]
    pub fn to_protocol(&self) -> rift_protocol::index::StoredEdge {
        rift_protocol::index::StoredEdge {
            producer: None,
            from: rift_protocol::read::SymbolId(self.from.as_str().to_owned()),
            to: rift_protocol::read::SymbolId(self.to.as_str().to_owned()),
            kind: rift_protocol::read::ExactKind(self.kind.as_ref().to_owned()),
            kinds: vec![self.kind],
            occurrence: Some(rift_protocol::read::SourceUnitSpan {
                unit: rift_protocol::read::SourceUnitId(self.occurrence.unit().to_string()),
                range: rift_protocol::read::TextRange {
                    start: self.occurrence.range().start(),
                    end: self.occurrence.range().end(),
                },
            }),
            provider: rift_protocol::read::EdgeProvider::Static,
            derivation: self.derivation,
            confidence: None,
            extensions: rift_protocol::read::Extensions::default(),
        }
    }
    /// The enclosing definition the edge starts at.
    #[must_use]
    pub const fn from(&self) -> &SymbolId {
        &self.from
    }

    /// The established symbol the edge points at.
    #[must_use]
    pub const fn to(&self) -> &SymbolId {
        &self.to
    }

    /// The edge's portable category, derived from the reference's role.
    #[must_use]
    pub const fn kind(&self) -> EdgeKind {
        self.kind
    }

    /// Where the reference occurred: source unit and byte range.
    #[must_use]
    pub const fn occurrence(&self) -> &DeclarationBinding {
        &self.occurrence
    }

    /// The authored static callee range used to compare independently resolved call sites.
    #[must_use]
    pub const fn static_callee(&self) -> Option<rift_syntax::ByteRange> {
        self.static_callee
    }
}

/// Each unit's definition ranges, resolving which definition encloses one reference
/// occurrence.
///
/// Built once from every contribution [`NormalizedGraph::records`] holds that carries a
/// `Contribution::source` binding - only a definition's own declaring contribution sets
/// that binding, so this is exactly the set of definitions with a placeable range.
/// Complexity is proportional to the graph's own record and contribution count, both
/// already bounded by [`PublicationLimits`](rift_provider::PublicationLimits).
struct UnitDefinitions {
    spans: DefinitionSpans,
    edge_modules: BTreeMap<SourceUnitId, SymbolId>,
}

struct DefinitionSpans(BTreeMap<SourceUnitId, EnclosingDefinitions<SymbolId>>);

#[cfg(test)]
thread_local! {
    static DEFINITION_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl UnitDefinitions {
    fn build(graph: &NormalizedGraph) -> Self {
        #[cfg(test)]
        DEFINITION_BUILDS.set(DEFINITION_BUILDS.get() + 1);
        let mut by_unit: BTreeMap<SourceUnitId, Vec<(u64, u64, SymbolId)>> = BTreeMap::new();
        let mut parents = BTreeMap::<SourceUnitId, &str>::new();
        let mut declared_edge_modules = BTreeMap::<SourceUnitId, SymbolId>::new();
        for record in graph.records() {
            let Some(identity) = record.identity() else {
                continue;
            };
            // The record's Contributions are read once: the module facts and the parent
            // identity belong to the record, and reading them per declaring binding
            // visited every Contribution again and parsed the identity again.
            let contributions = record
                .contributions()
                .iter()
                .filter_map(|key| graph.contribution(key))
                .collect::<Vec<_>>();
            let mut declared = None;
            for binding in contributions
                .iter()
                .filter_map(|contribution| contribution.source())
            {
                let range = binding.range();
                let (is_module, parent) = declared.get_or_insert_with(|| {
                    let is_module = contributions
                        .iter()
                        .filter_map(|contribution| contribution.facts())
                        .any(|facts| {
                            facts.language().name == "python" && facts.kind().0 == "module"
                        });
                    (
                        is_module,
                        identity
                            .as_str()
                            .rsplit_once('/')
                            .map_or("", |(parent, _)| parent),
                    )
                });
                if *is_module {
                    // The module owns occurrences outside its declarations. An exact
                    // declaration keeps its owner when its range covers the whole unit.
                    declared_edge_modules.insert(binding.unit().clone(), identity.clone());
                    continue;
                }
                parents
                    .entry(binding.unit().clone())
                    .and_modify(|common| *common = common_module_path(common, parent))
                    .or_insert(*parent);
                by_unit.entry(binding.unit().clone()).or_default().push((
                    range.start(),
                    range.end(),
                    identity.clone(),
                ));
            }
        }
        let by_unit = by_unit
            .into_iter()
            .map(|(unit, spans)| (unit, EnclosingDefinitions::new(spans)))
            .collect();
        // Every established identity came from a canonical anchor. A literal slash
        // separates whole encoded segments. Validate the final prefix once per unit:
        // a shared owner or language alone does not name a module.
        let mut edge_modules = parents
            .into_iter()
            .filter_map(|(unit, parent)| {
                rift_protocol::identity::SymbolIdentity::parse(parent).ok()?;
                Some((unit, SymbolId::new(parent).ok()?))
            })
            .collect::<BTreeMap<_, _>>();
        edge_modules.extend(declared_edge_modules);
        Self {
            spans: DefinitionSpans(by_unit),
            edge_modules,
        }
    }
}

impl DefinitionSpans {
    /// The innermost definition enclosing the occurrence, then its unit's module.
    fn resolve<'definitions>(
        &'definitions self,
        edge_modules: &'definitions BTreeMap<SourceUnitId, SymbolId>,
        occurrence: &DeclarationBinding,
    ) -> Option<&'definitions SymbolId> {
        let range = occurrence.range();
        self.0
            .get(occurrence.unit())
            .and_then(|definitions| definitions.resolve(range.start(), range.end()))
            .or_else(|| edge_modules.get(occurrence.unit()))
    }
}

/// The longest shared prefix ending at a whole encoded path segment.
fn common_module_path<'a>(left: &'a str, right: &str) -> &'a str {
    if left == right {
        return left;
    }
    let shared_bytes = left
        .split('/')
        .zip(right.split('/'))
        .take_while(|(left, right)| left == right)
        .map(|(segment, _)| segment.len() + 1)
        .sum::<usize>();
    &left[..shared_bytes.saturating_sub(1)]
}

/// Maps one reference's portable [`ReferenceRole`] onto the wire [`EdgeKind`] this
/// store's edges carry. `Unknown` - a provider exposing no sharper role - and every role this
/// mapping has no sharper kind for falls back to `References`, the kind's general-mention
/// sense.
const fn role_kind(role: ReferenceRole) -> EdgeKind {
    match role {
        ReferenceRole::Definition => EdgeKind::Declares,
        ReferenceRole::Read => EdgeKind::Reads,
        ReferenceRole::Write => EdgeKind::Writes,
        ReferenceRole::Import => EdgeKind::Imports,
        ReferenceRole::Call => EdgeKind::Calls,
        ReferenceRole::Type => EdgeKind::HasType,
        ReferenceRole::Unknown => EdgeKind::References,
    }
}

/// The edges one reference contributes: one per [`NormalizedTarget::Symbol`] target. A
/// [`NormalizedTarget::Contribution`] target - the graph never resolved it to an established
/// symbol - contributes no edge.
fn reference_edges<'graph>(
    reference: &'graph NormalizedReference,
    from: &'graph SymbolId,
    kind: EdgeKind,
) -> impl Iterator<Item = Edge> + 'graph {
    reference.targets().iter().filter_map(move |target| {
        let NormalizedTarget::Symbol(to) = target else {
            return None;
        };
        Some(Edge {
            from: from.clone(),
            to: to.clone(),
            kind,
            occurrence: reference.binding().clone(),
            static_callee: None,
            derivation: rift_protocol::read::EdgeDerivation::Resolution,
            from_graph: false,
            file_module_reference: FileModuleReference::None,
        })
    })
}

/// Orders two edges sharing one adjacency-list key by their other endpoint, then kind, then
/// occurrence position - so an outgoing list (constant `from`) orders by `to` and an
/// incoming list (constant `to`) orders by `from`, and two edges to the same neighbor from
/// different call sites still land in a stable order.
fn edge_order(left: &Edge, right: &Edge) -> Ordering {
    (
        &left.from,
        &left.to,
        left.kind,
        left.occurrence.unit(),
        left.occurrence.range().start(),
        left.occurrence.range().end(),
    )
        .cmp(&(
            &right.from,
            &right.to,
            right.kind,
            right.occurrence.unit(),
            right.occurrence.range().start(),
            right.occurrence.range().end(),
        ))
}

/// Bidirectional symbol reference adjacency for one normalized graph.
///
/// See the module doc for what an edge carries and how `from`, `kind`, and truncation are
/// derived.
#[derive(Debug, PartialEq, Eq)]
pub struct EdgeStore {
    outgoing: BTreeMap<SymbolId, Vec<Edge>>,
    incoming: BTreeMap<SymbolId, Vec<Edge>>,
    dropped_edges: u64,
    /// Whether static resolution ended on a work budget before it read every site.
    resolution_ended: bool,
    edge_modules: BTreeMap<SourceUnitId, SymbolId>,
}

impl get_size2::GetSize for Edge {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: get_size2::GetSizeTracker>(
        &self,
        mut tracker: T,
    ) -> (usize, T) {
        let mut bytes: usize = 0;
        let (field, next) = get_size2::GetSize::get_heap_size_with_tracker(&self.from, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = get_size2::GetSize::get_heap_size_with_tracker(&self.to, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) =
            get_size2::GetSize::get_heap_size_with_tracker(&self.occurrence, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        (bytes, tracker)
    }
}

impl get_size2::GetSize for EdgeStore {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (outgoing, tracker) = rift_protocol::retained_map_material(
            &self.outgoing,
            tracker,
            rift_protocol::retained_vector_material,
        );
        let (incoming, tracker) = rift_protocol::retained_map_material(
            &self.incoming,
            tracker,
            rift_protocol::retained_vector_material,
        );
        let (modules, tracker) =
            get_size2::GetSize::get_heap_size_with_tracker(&self.edge_modules, tracker);
        (
            outgoing.saturating_add(incoming).saturating_add(modules),
            tracker,
        )
    }
}

/// Edges of static occurrences whose targets a dependency or standard library owns.
fn admit_external<'a>(
    external: &'a [ExternalOccurrence],
    spans: &'a DefinitionSpans,
    edge_modules: &'a BTreeMap<SourceUnitId, SymbolId>,
    admitted: &mut BTreeMap<&'a SymbolId, Vec<Edge>>,
) {
    for occurrence in external {
        let Some(from) = occurrence
            .edge_from
            .as_ref()
            .or_else(|| spans.resolve(edge_modules, &occurrence.binding))
        else {
            continue;
        };
        let edge = Edge {
            from: from.clone(),
            to: occurrence.target.clone(),
            kind: occurrence
                .edge_kind
                .unwrap_or_else(|| role_kind(occurrence.role)),
            occurrence: occurrence.binding.clone(),
            static_callee: (occurrence.role == ReferenceRole::Call)
                .then_some(occurrence.callee_range),
            from_graph: false,
            file_module_reference: FileModuleReference::None,
            derivation: if occurrence.declared {
                rift_protocol::read::EdgeDerivation::Resolution
            } else {
                rift_protocol::read::EdgeDerivation::Syntax
            },
        };
        admitted.entry(from).or_default().push(edge);
    }
}

fn admit_occurrences<'a>(
    occurrences: &'a [StaticOccurrence],
    spans: &'a DefinitionSpans,
    edge_modules: &'a BTreeMap<SourceUnitId, SymbolId>,
    admitted: &mut BTreeMap<&'a SymbolId, Vec<Edge>>,
    file_module_reference: FileModuleReference,
) {
    for occurrence in occurrences {
        if occurrence.edge_kind.is_some() && occurrence.edge_from.is_none() {
            continue;
        }
        let NormalizedTarget::Symbol(to) = &occurrence.target else {
            continue;
        };
        let Some(from) = occurrence
            .edge_from
            .as_ref()
            .or_else(|| spans.resolve(edge_modules, &occurrence.binding))
        else {
            continue;
        };
        let edge = Edge {
            from: from.clone(),
            to: to.clone(),
            kind: occurrence
                .edge_kind
                .unwrap_or_else(|| role_kind(occurrence.role)),
            occurrence: occurrence.binding.clone(),
            static_callee: (occurrence.role == ReferenceRole::Call)
                .then_some(occurrence.callee_range),
            from_graph: false,
            file_module_reference,
            derivation: if occurrence.edge_kind.is_some() {
                rift_protocol::read::EdgeDerivation::Syntax
            } else {
                rift_protocol::read::EdgeDerivation::Resolution
            },
        };
        admitted.entry(from).or_default().push(edge);
    }
}

fn static_tail_starts(
    resolution: &mut StaticResolution,
    alias: Option<usize>,
    value_qualifier: Option<usize>,
) -> [Option<usize>; 64] {
    [
        resolution.late_supplemental_start.take(),
        resolution.python_reassignment_start.take(),
        resolution.python_capture_start.take(),
        resolution.python_receiver_start.take(),
        resolution.python_variadic_start.take(),
        resolution.python_initializer_start.take(),
        resolution.python_quoted_type_start.take(),
        resolution.python_overload_parameter_start.take(),
        resolution.file_module_start.take(),
        resolution.file_module_qualifier_start.take(),
        resolution.literal_start.take(),
        resolution.callback_parameter_start.take(),
        resolution.overload_parameter_start.take(),
        resolution.javascript_method_parameter_start.take(),
        resolution.scoped_javascript_function_start.take(),
        resolution.scoped_javascript_lexical_start.take(),
        resolution.typescript_type_parameter_start.take(),
        resolution.javascript_shorthand_parameter_start.take(),
        resolution.javascript_wrapped_parameter_start.take(),
        resolution.javascript_destructured_parameter_start.take(),
        resolution.typescript_union_property_start.take(),
        resolution.typescript_method_type_parameter_start.take(),
        resolution.typescript_function_type_parameter_start.take(),
        resolution.typescript_destructured_parameter_start.take(),
        alias,
        resolution.rust_receiver_start.take(),
        resolution.rust_generic_start.take(),
        resolution.rust_library_root_start.take(),
        resolution.rust_crate_paths_start.take(),
        resolution.rust_crate_visibility_start.take(),
        resolution.rust_argument_value_start.take(),
        resolution.rust_import_qualifier_start.take(),
        resolution.rust_shorthand_start.take(),
        resolution.rust_shared_start.take(),
        resolution.rust_parent_module_start.take(),
        resolution.python_comprehension_start.take(),
        resolution.javascript_callback_pattern_start.take(),
        resolution.javascript_lexical_alias_call_start.take(),
        resolution.javascript_assigned_member_start.take(),
        resolution.typescript_signature_type_parameter_start.take(),
        resolution.typescript_local_export_start.take(),
        value_qualifier,
        resolution
            .typescript_declared_signature_type_parameter_start
            .take(),
        resolution.typescript_inference_parameter_start.take(),
        resolution.python_property_receiver_start.take(),
        resolution.python_annotation_qualifier_start.take(),
        resolution.python_direct_annotation_qualifier_start.take(),
        resolution.python_wildcard_module_start.take(),
        resolution.python_legacy_annotation_qualifier_start.take(),
        resolution.python_loop_read_start.take(),
        resolution.commonjs_export_import_start.take(),
        resolution.typescript_incomplete_local_export_start.take(),
        resolution.typescript_imported_export_start.take(),
        resolution.typescript_inline_import_type_start.take(),
        resolution.commonjs_argument_import_start.take(),
        resolution.typescript_inline_import_module_start.take(),
        resolution.typescript_namespace_type_start.take(),
        resolution.javascript_wrapped_arrow_read_start.take(),
        resolution.typescript_enum_qualifier_start.take(),
        resolution.javascript_call_object_selector_start.take(),
        resolution.typescript_namespace_local_export_start.take(),
        resolution.python_dataclass_initializer_start.take(),
        resolution.python_assignment_expression_start.take(),
        resolution.typescript_property_selector_start.take(),
        resolution.python_lambda_parameter_start.take(),
    ]
}

fn static_admission_batches<'a>(
    spans: &'a DefinitionSpans,
    edge_modules: &'a BTreeMap<SourceUnitId, SymbolId>,
    resolution: &'a mut StaticResolution,
) -> [BTreeMap<&'a SymbolId, Vec<Edge>>; 66] {
    let supplemental_start = resolution.supplemental_start.take();
    let (value_qualifier, value_qualifier_external) = resolution
        .python_value_qualifier_start
        .take()
        .map_or((None, resolution.external.len()), |(internal, external)| {
            (Some(internal), external)
        });
    let (alias, alias_external) = resolution
        .rust_extern_alias_start
        .take()
        .map_or((None, value_qualifier_external), |(internal, external)| {
            (Some(internal), external)
        });
    let tail_starts = static_tail_starts(resolution, alias, value_qualifier);
    let mut boundaries = [0; 67];
    let mut next = resolution.occurrences.len();
    boundaries[66] = next;
    for (boundary, start) in boundaries[2..66].iter_mut().zip(tail_starts).rev() {
        next = start.unwrap_or(next);
        *boundary = next;
    }
    let (internal, external) = supplemental_start.unwrap_or((boundaries[2], alias_external));
    boundaries[1] = internal;
    let (ordinary_external, qualifier_external_rows) =
        resolution.external.split_at(value_qualifier_external);
    let (incumbent_external, alias_external_rows) = ordinary_external.split_at(alias_external);
    let (primary_external, supplemental_external) = incumbent_external.split_at(external);
    let mut batches = std::array::from_fn(|index| {
        let rows = &resolution.occurrences[boundaries[index]..boundaries[index + 1]];
        let mut edges = BTreeMap::new();
        admit_occurrences(
            rows,
            spans,
            edge_modules,
            &mut edges,
            match index {
                10 => FileModuleReference::Specifier,
                11 | 42 => FileModuleReference::Binding,
                52 => FileModuleReference::CommonJsExportImport,
                53 => FileModuleReference::IncompleteLocalExport,
                54 => FileModuleReference::ImportedExportSelector,
                55 => FileModuleReference::InlineImportType,
                56 => FileModuleReference::CommonJsArgumentImport,
                57 => FileModuleReference::InlineImportModule,
                _ => FileModuleReference::None,
            },
        );
        edges
    });
    admit_external(primary_external, spans, edge_modules, &mut batches[0]);
    admit_external(supplemental_external, spans, edge_modules, &mut batches[1]);
    admit_external(alias_external_rows, spans, edge_modules, &mut batches[26]);
    admit_external(
        qualifier_external_rows,
        spans,
        edge_modules,
        &mut batches[43],
    );
    // New ECMAScript references use the capacity left by the earlier stages.
    batches[15..36].rotate_left(11);
    // Preserve the established Rust and ECMAScript order ahead of newer Rust sites.
    batches[10..22].rotate_left(5);
    batches
}

impl EdgeStore {
    fn add_static(
        &mut self,
        spans: &DefinitionSpans,
        resolution: &mut StaticResolution,
        edges_max: usize,
    ) {
        if resolution.occurrences.is_empty() && resolution.external.is_empty() {
            return;
        }
        let existing = self.outgoing.values().map(Vec::len).sum::<usize>();
        let mut remaining = edges_max.saturating_sub(existing);
        let mut changed = false;
        let mut batches = static_admission_batches(spans, &self.edge_modules, resolution);
        self.deduplicate_static_batches(&mut batches);
        for (from, mut edges) in batches.into_iter().flatten() {
            let fits = edges.len().min(remaining);
            self.dropped_edges = self
                .dropped_edges
                .saturating_add(u64::try_from(edges.len() - fits).unwrap_or(u64::MAX));
            remaining -= fits;
            changed |= fits > 0;
            edges.truncate(fits);
            if edges.is_empty() {
                continue;
            }
            // The admitted list becomes the outgoing list's tail: one copy per direction.
            for edge in &edges {
                self.incoming
                    .entry(edge.to.clone())
                    .or_default()
                    .push(edge.clone());
            }
            self.outgoing
                .entry(from.clone())
                .or_default()
                .append(&mut edges);
        }
        if changed {
            for edges in self.outgoing.values_mut().chain(self.incoming.values_mut()) {
                edges.sort_by(edge_order);
                edges.dedup();
            }
        }
    }

    fn deduplicate_static_batches(&self, batches: &mut [BTreeMap<&SymbolId, Vec<Edge>>]) {
        for position in 0..batches.len() {
            let (prior, next) = batches.split_at_mut(position);
            for (from, edges) in &mut next[0] {
                edges.sort_by(edge_order);
                edges.dedup();
                let existing = self.outgoing(from);
                // A shared row counts in the first batch even when the cap drops it.
                edges.retain(|edge| {
                    std::iter::once(existing)
                        .chain(
                            prior
                                .iter()
                                .filter_map(|batch| batch.get(from).map(Vec::as_slice)),
                        )
                        .all(|earlier| {
                            earlier
                                .binary_search_by(|other| edge_order(other, edge))
                                .is_err()
                        })
                });
            }
        }
    }

    /// Builds the adjacency from every reference in `graph`, capped at
    /// [`EDGES_MAX`].
    #[must_use]
    pub fn build(graph: &NormalizedGraph) -> Self {
        Self::build_capped(graph, EDGES_MAX)
    }

    /// Builds adjacency under the caller's edge cap, reporting every omitted edge.
    /// A zero cap publishes no edges.
    #[must_use]
    pub fn build_capped(graph: &NormalizedGraph, edges_max: usize) -> Self {
        Self::build_placed(graph, BTreeMap::new(), edges_max)
    }

    /// Builds graph adjacency whose units' modules are `modules` where placement
    /// established one, else the common parent of the unit's declarations.
    pub(crate) fn build_placed(
        graph: &NormalizedGraph,
        modules: BTreeMap<SourceUnitId, SymbolId>,
        edges_max: usize,
    ) -> Self {
        Self::build_with_spans(graph, modules, edges_max).0
    }

    /// Builds graph adjacency and adds static edges with the same definition spans.
    pub(crate) fn build_resolving(
        graph: &NormalizedGraph,
        modules: BTreeMap<SourceUnitId, SymbolId>,
        edges_max: usize,
        resolve: impl FnOnce() -> StaticResolution,
    ) -> (Self, StaticResolution) {
        let (mut store, spans) = Self::build_with_spans(graph, modules, edges_max);
        let mut resolution = resolve();
        store.resolution_ended = resolution.ended;
        store.add_static(&spans, &mut resolution, edges_max);
        (store, resolution)
    }

    fn build_with_spans(
        graph: &NormalizedGraph,
        modules: BTreeMap<SourceUnitId, SymbolId>,
        edges_max: usize,
    ) -> (Self, DefinitionSpans) {
        let UnitDefinitions {
            spans,
            mut edge_modules,
        } = UnitDefinitions::build(graph);
        // A declaration can belong to a type of another module, so the module placement
        // established decides before the common parent of the declarations.
        edge_modules.extend(modules);
        let mut outgoing: BTreeMap<SymbolId, Vec<Edge>> = BTreeMap::new();
        let mut incoming: BTreeMap<SymbolId, Vec<Edge>> = BTreeMap::new();
        let mut budget = LoopBudget::new(edges_max);
        let mut dropped_edges: u64 = 0;
        for reference in graph.references() {
            let Some(from) = spans.resolve(&edge_modules, reference.binding()) else {
                continue;
            };
            let kind = role_kind(reference.role());
            for edge in reference_edges(reference, from, kind) {
                match budget.consume() {
                    Ok(()) => {
                        outgoing
                            .entry(edge.from.clone())
                            .or_default()
                            .push(edge.clone());
                        incoming.entry(edge.to.clone()).or_default().push(edge);
                    }
                    Err(_exhausted) => dropped_edges += 1,
                }
            }
        }
        for edge in graph.edges() {
            let Some(from) = graph.identity_for(edge.source().reference()) else {
                continue;
            };
            let NormalizedTarget::Symbol(to) = edge.target() else {
                continue;
            };
            let Some(occurrence) = edge.occurrence() else {
                continue;
            };
            let Some(derivation) = edge
                .derivation()
                .filter(|derivation| *derivation != rift_protocol::read::EdgeDerivation::Heuristic)
            else {
                continue;
            };
            match budget.consume() {
                Ok(()) => {
                    let edge = Edge {
                        from: from.clone(),
                        to: to.clone(),
                        kind: crate::analyzer::edge_kind(edge.kind()),
                        occurrence: occurrence.clone(),
                        static_callee: None,
                        derivation,
                        from_graph: true,
                        file_module_reference: FileModuleReference::None,
                    };
                    outgoing.entry(from.clone()).or_default().push(edge.clone());
                    incoming.entry(to.clone()).or_default().push(edge);
                }
                Err(_) => dropped_edges += 1,
            }
        }
        for edges in outgoing.values_mut() {
            edges.sort_by(edge_order);
        }
        for edges in incoming.values_mut() {
            edges.sort_by(edge_order);
        }
        (
            Self {
                outgoing,
                incoming,
                dropped_edges,
                resolution_ended: false,
                edge_modules,
            },
            spans,
        )
    }

    /// Canonical source module used by references outside every declaration.
    #[must_use]
    pub fn edge_module(&self, unit: &SourceUnitId) -> Option<&SymbolId> {
        self.edge_modules.get(unit)
    }

    /// Every captured edge, ordered by source and target.
    pub fn edges(&self) -> impl Iterator<Item = &Edge> {
        self.outgoing.values().flat_map(|edges| edges.iter())
    }

    /// Edges starting at `from`, in deterministic order. Empty when `from` has none.
    #[must_use]
    pub fn outgoing(&self, from: &SymbolId) -> &[Edge] {
        self.outgoing.get(from).map_or(&[], Vec::as_slice)
    }

    /// Edges pointing at `to`, in deterministic order. Empty when `to` has none.
    #[must_use]
    pub fn incoming(&self, to: &SymbolId) -> &[Edge] {
        self.incoming.get(to).map_or(&[], Vec::as_slice)
    }

    /// Whether every eligible reference became an edge. `false` once building stopped at
    /// the configured edge cap, or static resolution ended on a work budget.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.dropped_edges == 0 && !self.resolution_ended
    }

    /// Whether this store holds no edges at all. `outgoing` and `incoming` always gain an
    /// entry together per edge, so one map empty means both are.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.outgoing.is_empty()
    }

    /// Edges building skipped once the cap was crossed.
    #[must_use]
    pub const fn dropped_edges(&self) -> u64 {
        self.dropped_edges
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rift_core::{
        Contribution, ContributionKey, ContributionOrigin, ContributionReference,
        DeclarationBinding, ExactKind, IndexRevision, Language, PortableSymbolFacts, ProviderId,
        ProviderRevision, ProviderSymbolId, SourceApplicability, SourceKind, SourceLocation,
        SourcePath, SourceRange, SourceResolverId, SourceRevision, SourceUnitId, SymbolId,
        TreeRevision,
    };
    use rift_provider::{
        NormalizedGraph, Normalizer, ProviderPublication, PublicationLimits, PublicationSet,
    };

    use super::{EdgeKind, EdgeStore, ReferenceRole, role_kind};

    fn provider(value: &str) -> ProviderId {
        ProviderId::new(value).expect("fixture provider identity")
    }

    fn source_unit(path: &str) -> SourceUnitId {
        SourceUnitId::new(
            SourceResolverId::new("project").expect("fixture resolver identity"),
            SourcePath::new(path).expect("fixture source path"),
        )
        .expect("fixture source unit")
    }

    fn binding(path: &str, start: u64, end: u64) -> DeclarationBinding {
        DeclarationBinding::new(
            source_unit(path),
            SourceRange::new(start, end).expect("fixture range"),
        )
    }

    fn exact() -> SourceApplicability {
        SourceApplicability::Exact {
            source_revision: SourceRevision::new(1).expect("fixture source revision"),
            tree_revision: TreeRevision::new(1).expect("fixture tree revision"),
        }
    }

    fn authored_origin() -> ContributionOrigin {
        ContributionOrigin::new(
            Some(SourceLocation::Project { package: None }),
            SourceKind::Authored,
        )
        .expect("fixture origin")
    }

    fn fixture_symbol(name: &str) -> SymbolId {
        let identity = rift_protocol::identity::SymbolIdentity::new(
            rift_protocol::identity::SymbolOwner::Local,
            Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            vec!["fixture".to_owned(), name.to_owned()],
        )
        .expect("canonical fixture identity");
        SymbolId::new(identity.wire_identity()).expect("fixture symbol")
    }

    /// One definition contribution: an established identity, anchored at `range` in `path`.
    fn definition(symbol: &str, identity: &str, path: &str, range: (u64, u64)) -> Contribution {
        let facts = PortableSymbolFacts::new(
            Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            identity,
            identity,
            ExactKind("function".to_owned()),
        );
        Contribution::builder(
            ContributionKey::new(
                provider("syntax"),
                ProviderRevision::new(1).expect("fixture provider revision"),
                ProviderSymbolId::new(symbol).expect("fixture provider symbol"),
            ),
            exact(),
            facts,
            authored_origin(),
        )
        .source(binding(path, range.0, range.1))
        .identity_anchor(SymbolId::new(identity).expect("fixture identity"))
        .build()
        .expect("fixture definition contribution")
    }

    /// One reference-only contribution, carrying no facts and no source binding of its own -
    /// production `reference_contributions` builds them the same way.
    fn reference(
        key_symbol: &str,
        occurrence: DeclarationBinding,
        role: ReferenceRole,
        target: &str,
    ) -> Contribution {
        let target = ContributionReference::new(
            provider("syntax"),
            ProviderSymbolId::new(target).expect("fixture target symbol"),
        );
        let semantic = rift_core::SemanticReference::new(occurrence, role, vec![target])
            .expect("fixture semantic reference");
        Contribution::fact_builder(
            ContributionKey::new(
                provider("syntax"),
                ProviderRevision::new(1).expect("fixture provider revision"),
                ProviderSymbolId::new(key_symbol).expect("fixture provider symbol"),
            ),
            exact(),
            authored_origin(),
        )
        .references(vec![semantic])
        .build()
        .expect("fixture reference contribution")
    }

    fn graph(contributions: Vec<Contribution>) -> NormalizedGraph {
        let publication = ProviderPublication::new(
            provider("syntax"),
            ProviderRevision::new(1).expect("fixture provider revision"),
            contributions,
            PublicationLimits::default(),
        )
        .expect("fixture publication");
        let publications = Arc::new(
            PublicationSet::empty(PublicationLimits::default())
                .replaced(publication)
                .expect("fixture publication set"),
        );
        Normalizer::normalize(
            IndexRevision::new(1).expect("fixture index revision"),
            SourceRevision::new(1).expect("fixture source revision"),
            TreeRevision::new(1).expect("fixture tree revision"),
            &publications,
            None,
        )
        .expect("fixture normalized graph")
    }

    fn symbol(text: &str) -> SymbolId {
        SymbolId::new(text).expect("fixture symbol identity")
    }

    #[test]
    fn role_kind_maps_every_reference_role() {
        let cases = [
            (ReferenceRole::Definition, EdgeKind::Declares),
            (ReferenceRole::Read, EdgeKind::Reads),
            (ReferenceRole::Write, EdgeKind::Writes),
            (ReferenceRole::Import, EdgeKind::Imports),
            (ReferenceRole::Call, EdgeKind::Calls),
            (ReferenceRole::Type, EdgeKind::HasType),
            (ReferenceRole::Unknown, EdgeKind::References),
        ];
        for (role, expected) in cases {
            assert_eq!(
                role_kind(role),
                expected,
                "role {role:?} maps to {expected:?}"
            );
        }
    }

    #[test]
    fn reference_inside_a_definition_yields_an_edge_both_directions_agree() {
        let occurrence = binding("src/lib.rs", 30, 35);
        let contributions = vec![
            definition(
                "beta",
                fixture_symbol("beta").as_str(),
                "src/lib.rs",
                (0, 40),
            ),
            definition(
                "alpha",
                fixture_symbol("alpha").as_str(),
                "src/lib.rs",
                (40, 60),
            ),
            reference("beta_ref_alpha", occurrence, ReferenceRole::Call, "alpha"),
        ];
        let store = EdgeStore::build(&graph(contributions));

        let beta = symbol(fixture_symbol("beta").as_str());
        let alpha = symbol(fixture_symbol("alpha").as_str());
        let outgoing = store.outgoing(&beta);
        let incoming = store.incoming(&alpha);
        assert_eq!(outgoing.len(), 1);
        assert_eq!(incoming.len(), 1);
        assert_eq!(outgoing[0], incoming[0]);
        assert_eq!(outgoing[0].from(), &beta);
        assert_eq!(outgoing[0].to(), &alpha);
        assert_eq!(outgoing[0].kind(), EdgeKind::Calls);
        assert!(store.is_complete());
        assert_eq!(store.dropped_edges(), 0);
    }

    #[test]
    fn enclosing_definition_resolution_picks_the_innermost_definition() {
        let occurrence = binding("src/lib.rs", 20, 25);
        let contributions = vec![
            definition(
                "outer",
                fixture_symbol("outer").as_str(),
                "src/lib.rs",
                (0, 60),
            ),
            definition(
                "inner",
                fixture_symbol("inner").as_str(),
                "src/lib.rs",
                (10, 40),
            ),
            definition(
                "helper",
                fixture_symbol("helper").as_str(),
                "src/lib.rs",
                (60, 80),
            ),
            reference(
                "inner_ref_helper",
                occurrence,
                ReferenceRole::Call,
                "helper",
            ),
        ];
        let store = EdgeStore::build(&graph(contributions));

        let outer = symbol(fixture_symbol("outer").as_str());
        let inner = symbol(fixture_symbol("inner").as_str());
        assert!(
            store.outgoing(&outer).is_empty(),
            "the outer definition gets no edge"
        );
        assert_eq!(
            store.outgoing(&inner).len(),
            1,
            "the inner definition gets the edge"
        );
        assert_eq!(store.outgoing(&inner)[0].from(), &inner);
    }

    #[test]
    fn reference_outside_every_definition_yields_no_edge() {
        let occurrence = binding("src/lib.rs", 100, 105);
        let contributions = vec![
            definition(
                "alpha",
                "rift://symbol/rust/src/lib.rs/alpha",
                "src/lib.rs",
                (0, 40),
            ),
            reference(
                "module_ref_alpha",
                occurrence,
                ReferenceRole::Import,
                "alpha",
            ),
        ];
        let store = EdgeStore::build(&graph(contributions));

        let alpha = symbol("rift://symbol/rust/src/lib.rs/alpha");
        assert!(store.incoming(&alpha).is_empty());
        assert!(store.is_complete());
    }

    #[test]
    fn edge_module_keeps_only_shared_qualified_segments() {
        let path = "src/lib.rs";
        let unit = source_unit(path);
        let cases = [
            (
                "rift://symbol/local/rust/crate/a%2Fb/one",
                "rift://symbol/local/rust/crate/a%2Fb/two",
                Some("rift://symbol/local/rust/crate/a%2Fb"),
            ),
            (
                "rift://symbol/local/rust/crate/mod/one",
                "rift://symbol/local/rust/crate/modx/two",
                Some("rift://symbol/local/rust/crate"),
            ),
            (
                "rift://symbol/local/rust/crate/one",
                "rift://symbol/local/rust/crate/child/two",
                Some("rift://symbol/local/rust/crate"),
            ),
            (
                "rift://symbol/local/rust/one",
                "rift://symbol/local/rust/two",
                None,
            ),
            (
                "rift://symbol/local/rust/crate/one",
                "rift://symbol/local@other/rust/crate/two",
                None,
            ),
            (
                "rift://symbol/local/rust/crate/one",
                "rift://symbol/local/python/crate/two",
                None,
            ),
            (
                "rift://symbol/npm/npmjs.org/@types/node/typescript/node/one",
                "rift://symbol/npm/npmjs.org/@types/node/typescript/node/two",
                Some("rift://symbol/npm/npmjs.org/@types/node/typescript/node"),
            ),
            (
                "rift://symbol/stdlib/cpython/python/builtins/one",
                "rift://symbol/stdlib/cpython/python/builtins/two",
                Some("rift://symbol/stdlib/cpython/python/builtins"),
            ),
            (
                "rift://symbol/local/rust/crate/one",
                "rift://symbol/local/rust/invalid%2fanchor/two",
                Some("rift://symbol/local/rust/crate"),
            ),
        ];
        for (left, right, expected) in cases {
            let graph = graph(vec![
                definition("left", left, path, (0, 4)),
                definition("right", right, path, (5, 9)),
            ]);
            assert_eq!(
                EdgeStore::build(&graph)
                    .edge_module(&unit)
                    .map(SymbolId::as_str),
                expected
            );
        }
    }

    #[test]
    fn test_edge_module_scope_keeps_import_and_type_occurrences() {
        let alpha = fixture_symbol("alpha");
        let module = symbol("rift://symbol/local/rust/fixture");
        let store = EdgeStore::build(&graph(vec![
            definition("alpha", alpha.as_str(), "src/lib.rs", (0, 40)),
            reference(
                "edge_import",
                binding("src/lib.rs", 100, 105),
                ReferenceRole::Import,
                "alpha",
            ),
            reference(
                "edge_header",
                binding("src/lib.rs", 110, 115),
                ReferenceRole::Type,
                "alpha",
            ),
        ]));
        let edges = store.incoming(&alpha);
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|edge| edge.from() == &module));
        assert_eq!(store.edge_module(&source_unit("src/lib.rs")), Some(&module));
    }

    #[test]
    fn unresolved_reference_target_yields_no_edge_edge() {
        let caller = symbol("rift://symbol/rust/src/lib.rs/caller");
        let store = EdgeStore::build(&graph(vec![
            definition(
                "caller",
                "rift://symbol/rust/src/lib.rs/caller",
                "src/lib.rs",
                (0, 40),
            ),
            reference(
                "caller_ref_missing",
                binding("src/lib.rs", 5, 11),
                ReferenceRole::Call,
                "missing",
            ),
        ]));

        assert!(store.outgoing(&caller).is_empty());
        assert!(store.is_empty());
        assert!(store.is_complete());
    }

    #[test]
    fn building_the_same_graph_twice_produces_equal_stores() {
        let occurrence = binding("src/lib.rs", 30, 35);
        let contributions = vec![
            definition(
                "beta",
                "rift://symbol/rust/src/lib.rs/beta",
                "src/lib.rs",
                (0, 40),
            ),
            definition(
                "alpha",
                "rift://symbol/rust/src/lib.rs/alpha",
                "src/lib.rs",
                (40, 60),
            ),
            reference("beta_ref_alpha", occurrence, ReferenceRole::Call, "alpha"),
        ];
        let normalized = graph(contributions);
        assert_eq!(EdgeStore::build(&normalized), EdgeStore::build(&normalized));
    }

    #[test]
    fn supplemental_static_edges_use_only_the_capacity_left_by_primary_calls() {
        use super::{StaticOccurrence, StaticResolution};
        use rift_provider::NormalizedTarget;

        let path = "src/lib.rs";
        let alpha = fixture_symbol("alpha");
        let zeta = fixture_symbol("zeta");
        let callee = fixture_symbol("callee");
        let graph = graph(vec![
            definition("alpha", alpha.as_str(), path, (0, 20)),
            definition("zeta", zeta.as_str(), path, (20, 40)),
            definition("callee", callee.as_str(), path, (40, 60)),
        ]);
        let occurrence = |from: &SymbolId, start, role| StaticOccurrence {
            binding: binding(path, start, start + 1),
            target: NormalizedTarget::Symbol(callee.clone()),
            target_binding: None,
            source_digest: rift_core::FileDigest::of(b"supplemental fixture"),
            callee_range: rift_syntax::ByteRange {
                start,
                end: start + 1,
            },
            role,
            edge_from: Some(from.clone()),
            edge_kind: None,
        };
        let resolution = |batch_count| {
            let mut occurrences = vec![
                occurrence(&alpha, 5, ReferenceRole::Call),
                occurrence(&zeta, 25, ReferenceRole::Call),
            ];
            if batch_count >= 2 {
                occurrences.push(occurrence(&alpha, 3, ReferenceRole::Read));
                occurrences.push(occurrence(&zeta, 25, ReferenceRole::Call));
            }
            let late_start = (batch_count >= 3).then_some(occurrences.len());
            if late_start.is_some() {
                occurrences.push(occurrence(&alpha, 2, ReferenceRole::Read));
                occurrences.push(occurrence(&alpha, 3, ReferenceRole::Read));
            }
            let python_reassignment_start = (batch_count >= 4).then_some(occurrences.len());
            if python_reassignment_start.is_some() {
                occurrences.push(occurrence(&alpha, 1, ReferenceRole::Read));
                occurrences.push(occurrence(&alpha, 2, ReferenceRole::Read));
            }
            let python_capture_start = (batch_count >= 5).then_some(occurrences.len());
            if python_capture_start.is_some() {
                occurrences.push(occurrence(&alpha, 0, ReferenceRole::Read));
                occurrences.push(occurrence(&alpha, 1, ReferenceRole::Read));
            }
            let rust_import_qualifier_start = (batch_count >= 6).then_some(occurrences.len());
            if rust_import_qualifier_start.is_some() {
                occurrences.push(occurrence(&alpha, 0, ReferenceRole::Read));
                occurrences.push(occurrence(&alpha, 9, ReferenceRole::Read));
            }
            StaticResolution {
                occurrences,
                supplemental_start: (batch_count >= 2).then_some((2, 0)),
                late_supplemental_start: late_start,
                python_reassignment_start,
                python_capture_start,
                rust_import_qualifier_start,
                ..StaticResolution::default()
            }
        };
        let build = |cap, batch_count| {
            EdgeStore::build_resolving(&graph, super::BTreeMap::new(), cap, || {
                resolution(batch_count)
            })
            .0
        };
        let baseline = build(2, 1);
        assert!(baseline.is_complete());
        assert_eq!(baseline.edges().count(), 2);

        let expanded = build(2, 2);
        assert_eq!(expanded.dropped_edges(), 1);
        assert!(!expanded.is_complete());
        assert_eq!(expanded.outgoing(&zeta), baseline.outgoing(&zeta));
        assert_eq!(expanded.outgoing(&alpha), baseline.outgoing(&alpha));
        assert_eq!(expanded.incoming(&callee), baseline.incoming(&callee));
        assert_eq!(build(1, 2).dropped_edges(), 2);

        let room = build(3, 2);
        assert!(room.is_complete());
        assert_eq!(room.edges().count(), 3);
        assert_eq!(room.outgoing(&alpha).len(), 2);
        assert_eq!(room.outgoing(&zeta), baseline.outgoing(&zeta));

        for batch in 3..=6 {
            let earlier = build(batch, batch - 1);
            assert!(earlier.is_complete());
            let capped = build(batch, batch);
            assert_eq!(capped.dropped_edges(), 1);
            assert!(capped.edges().eq(earlier.edges()));
            let expanded = build(batch + 1, batch);
            assert!(expanded.is_complete());
            assert_eq!(expanded.edges().count(), batch + 1);
        }
    }

    #[test]
    fn shared_definitions_preserve_capped_graph_and_static_edge_admission() {
        use super::{
            DEFINITION_BUILDS, ExternalOccurrence, StaticOccurrence, StaticResolution,
            UnitDefinitions,
        };
        use rift_provider::NormalizedTarget;

        let containing_function = fixture_symbol("caller");
        let call_target = fixture_symbol("called");
        let tied = fixture_symbol("tied");
        let path = "src/lib.rs";
        let graph = graph(vec![
            definition("caller", containing_function.as_str(), path, (10, 40)),
            definition("called", call_target.as_str(), path, (40, 60)),
            definition("tied", tied.as_str(), path, (10, 40)),
            reference(
                "graph_call",
                binding(path, 20, 25),
                ReferenceRole::Call,
                "called",
            ),
            reference(
                "graph_import",
                binding(path, 62, 66),
                ReferenceRole::Import,
                "called",
            ),
        ]);
        let resolution = || {
            let occurrence = |start, end| StaticOccurrence {
                binding: binding(path, start, end),
                target: NormalizedTarget::Symbol(call_target.clone()),
                target_binding: None,
                source_digest: rift_core::FileDigest::of(b"fixture"),
                callee_range: rift_syntax::ByteRange { start, end },
                role: ReferenceRole::Call,
                edge_from: None,
                edge_kind: None,
            };
            StaticResolution {
                occurrences: vec![occurrence(20, 25), occurrence(27, 32), occurrence(2, 6)],
                external: vec![ExternalOccurrence {
                    binding: binding(path, 31, 37),
                    target: fixture_symbol("external"),
                    callee_range: rift_syntax::ByteRange { start: 31, end: 37 },
                    role: ReferenceRole::Call,
                    edge_from: None,
                    edge_kind: None,
                    declared: true,
                }],
                ..StaticResolution::default()
            }
        };
        let module = symbol("rift://symbol/local/rust/fixture");
        for cap in 0..=6 {
            let before = DEFINITION_BUILDS.get();
            let (shared, _) =
                EdgeStore::build_resolving(&graph, super::BTreeMap::new(), cap, resolution);
            assert_eq!(DEFINITION_BUILDS.get() - before, 1, "cap {cap}");

            let mut separate = EdgeStore::build_capped(&graph, cap);
            let fresh = UnitDefinitions::build(&graph);
            separate.add_static(&fresh.spans, &mut resolution(), cap);
            assert_eq!(DEFINITION_BUILDS.get() - before, 3, "cap {cap}");
            assert_eq!(
                shared, separate,
                "ordered edges in both directions at cap {cap}"
            );
            assert_eq!(shared.edges().count(), cap.min(5), "cap {cap}");
            // At zero, the graph edge that would filter the duplicate static call
            // never enters the store. Once both graph edges fit, only five edges remain.
            if cap == 0 {
                assert_eq!(shared.dropped_edges(), 6);
            } else if cap >= 2 {
                assert_eq!(shared.dropped_edges(), 5_u64.saturating_sub(cap as u64));
            }
            assert_eq!(shared.edge_module(&source_unit(path)), Some(&module));
            if cap == 5 {
                let rows = shared.incoming(&call_target);
                let inner = rows
                    .iter()
                    .find(|edge| edge.occurrence().range().start() == 20)
                    .expect("graph call resolves within tied spans")
                    .from();
                assert!(inner == &containing_function || inner == &tied);
                assert!(rows.iter().any(|edge| edge.from() == inner && edge.occurrence().range().start() == 27));
                assert!(
                    rows.iter().any(
                        |edge| edge.from() == &module && edge.occurrence().range().start() == 2
                    )
                );
                assert!(
                    rows.iter()
                        .any(|edge| edge.from() == &module
                            && edge.occurrence().range().start() == 62)
                );
            }
        }
    }

    #[test]
    fn building_stops_at_the_edge_cap_and_reports_the_drop() {
        let contributions = vec![
            definition(
                "beta",
                fixture_symbol("beta").as_str(),
                "src/lib.rs",
                (0, 40),
            ),
            definition(
                "alpha",
                fixture_symbol("alpha").as_str(),
                "src/lib.rs",
                (40, 60),
            ),
            definition(
                "gamma",
                fixture_symbol("gamma").as_str(),
                "src/lib.rs",
                (60, 80),
            ),
            reference(
                "beta_ref_alpha",
                binding("src/lib.rs", 5, 10),
                ReferenceRole::Call,
                "alpha",
            ),
            reference(
                "beta_ref_gamma",
                binding("src/lib.rs", 15, 20),
                ReferenceRole::Call,
                "gamma",
            ),
        ];
        let store = super::EdgeStore::build_capped(&graph(contributions), 1);

        assert!(!store.is_complete());
        assert_eq!(store.dropped_edges(), 1);
        let beta = symbol(fixture_symbol("beta").as_str());
        assert_eq!(
            store.outgoing(&beta).len(),
            1,
            "the cap keeps exactly one edge"
        );
    }

    #[test]
    fn a_self_call_yields_one_edge_listed_in_both_directions() {
        let occurrence = binding("src/lib.rs", 10, 17);
        let contributions = vec![
            definition(
                "recurse",
                fixture_symbol("recurse").as_str(),
                "src/lib.rs",
                (0, 40),
            ),
            reference("self_ref", occurrence, ReferenceRole::Call, "recurse"),
        ];
        let store = EdgeStore::build(&graph(contributions));

        let recurse = symbol(fixture_symbol("recurse").as_str());
        let outgoing = store.outgoing(&recurse);
        let incoming = store.incoming(&recurse);
        assert_eq!(outgoing.len(), 1, "outgoing={outgoing:?}");
        assert_eq!(incoming.len(), 1, "incoming={incoming:?}");
        assert_eq!(outgoing[0], incoming[0]);
        assert_eq!(outgoing[0].from(), outgoing[0].to());
    }

    #[test]
    fn two_call_sites_to_one_target_keep_two_edges_in_occurrence_order() {
        let contributions = vec![
            definition(
                "caller",
                fixture_symbol("caller").as_str(),
                "src/lib.rs",
                (0, 40),
            ),
            definition(
                "callee",
                fixture_symbol("callee").as_str(),
                "src/lib.rs",
                (40, 60),
            ),
            reference(
                "second_site",
                binding("src/lib.rs", 20, 26),
                ReferenceRole::Call,
                "callee",
            ),
            reference(
                "first_site",
                binding("src/lib.rs", 5, 11),
                ReferenceRole::Call,
                "callee",
            ),
        ];
        let store = EdgeStore::build(&graph(contributions));

        let from_symbol = symbol(fixture_symbol("caller").as_str());
        let to_symbol = symbol(fixture_symbol("callee").as_str());
        let outgoing = store.outgoing(&from_symbol);
        assert_eq!(outgoing.len(), 2, "outgoing={outgoing:?}");
        assert!(outgoing.iter().all(|edge| edge.to() == &to_symbol));
        assert_eq!(store.incoming(&to_symbol).len(), 2);
        let positions: Vec<u64> = outgoing
            .iter()
            .map(|edge| edge.occurrence().range().start())
            .collect();
        assert_eq!(
            positions,
            vec![5, 20],
            "edges sharing endpoint and kind order by occurrence position"
        );
    }
}
