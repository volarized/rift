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
//! Building stops at [`EDGES_MAX`] edges; [`EdgeStore::is_complete`] and
//! [`EdgeStore::dropped_edges`] report the truncation.

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

/// Most base graph endpoints carried through one static admission, across both directions.
const STATIC_FINALIZATION_BASE_KEYS_MAX: usize = 8_192;

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
}

impl Edge {
    /// Whether the normalized graph already carries this edge.
    pub(crate) const fn is_graph_edge(&self) -> bool {
        self.from_graph
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
        let mut edge_modules = BTreeMap::<SourceUnitId, Option<SymbolId>>::new();
        let mut declared_edge_modules = BTreeMap::<SourceUnitId, SymbolId>::new();
        let mut common_modules = CommonModules::default();
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
                    (is_module, edge_module_parent(identity))
                });
                if *is_module {
                    declared_edge_modules.insert(binding.unit().clone(), identity.clone());
                }
                edge_modules
                    .entry(binding.unit().clone())
                    .and_modify(|current| {
                        *current = current
                            .as_ref()
                            .zip(parent.as_ref())
                            .and_then(|(left, right)| common_modules.common(left, right));
                    })
                    .or_insert_with(|| parent.clone());
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
        let mut edge_modules = edge_modules
            .into_iter()
            .filter_map(|(unit, identity)| identity.map(|identity| (unit, identity)))
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

fn edge_module_parent(identity: &SymbolId) -> Option<SymbolId> {
    let identity = rift_protocol::identity::SymbolIdentity::parse(identity.as_str()).ok()?;
    let mut path = identity.qualified_path().to_vec();
    path.pop()?;
    let module = rift_protocol::identity::SymbolIdentity::new(
        identity.owner().clone(),
        identity.language().clone(),
        path,
    )
    .ok()?;
    SymbolId::new(module.wire_identity()).ok()
}

/// Answers of [`edge_module_common`] by identity pair. The declarations of one unit share
/// few distinct parents, and each answer parses both identities.
#[derive(Default)]
struct CommonModules(BTreeMap<SymbolId, BTreeMap<SymbolId, Option<SymbolId>>>);

impl CommonModules {
    fn common(&mut self, left: &SymbolId, right: &SymbolId) -> Option<SymbolId> {
        if let Some(known) = self.0.get(left).and_then(|answers| answers.get(right)) {
            return known.clone();
        }
        let common = edge_module_common(left, right);
        self.0
            .entry(left.clone())
            .or_default()
            .insert(right.clone(), common.clone());
        common
    }
}

fn edge_module_common(left: &SymbolId, right: &SymbolId) -> Option<SymbolId> {
    let left = rift_protocol::identity::SymbolIdentity::parse(left.as_str()).ok()?;
    let right = rift_protocol::identity::SymbolIdentity::parse(right.as_str()).ok()?;
    if left.owner() != right.owner() || left.language() != right.language() {
        return None;
    }
    let path = left
        .qualified_path()
        .iter()
        .zip(right.qualified_path())
        .take_while(|(left, right)| left == right)
        .map(|(part, _)| part.clone())
        .collect();
    let module = rift_protocol::identity::SymbolIdentity::new(
        left.owner().clone(),
        left.language().clone(),
        path,
    )
    .ok()?;
    SymbolId::new(module.wire_identity()).ok()
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
            derivation: if occurrence.declared {
                rift_protocol::read::EdgeDerivation::Resolution
            } else {
                rift_protocol::read::EdgeDerivation::Syntax
            },
        };
        admitted.entry(from).or_default().push(edge);
    }
}

struct GraphAdjacency {
    outgoing_lists: usize,
    incoming_lists: usize,
    dropped_edges: u64,
    rows: usize,
}

fn finalize_lists(
    lists: &mut BTreeMap<SymbolId, Vec<Edge>>,
    base_keys: Option<Vec<SymbolId>>,
    total_rows: usize,
) -> (usize, usize, usize, usize) {
    let finish = |edges: &mut Vec<Edge>| {
        let count = edges.len();
        edges.sort_by(edge_order);
        edges.dedup();
        count
    };
    let total_lists = lists.len();
    if let Some(keys) = base_keys {
        let finalized_lists = keys.len();
        let finalized_rows = keys
            .iter()
            .map(|key| finish(lists.get_mut(key).expect("admission keeps base lists")))
            .sum::<usize>();
        (
            finalized_lists,
            finalized_rows,
            total_lists - finalized_lists,
            total_rows - finalized_rows,
        )
    } else {
        let finalized_rows = lists.values_mut().map(finish).sum();
        (total_lists, finalized_rows, 0, 0)
    }
}

impl EdgeStore {
    fn add_static(
        &mut self,
        spans: &DefinitionSpans,
        occurrences: &[StaticOccurrence],
        external: &[ExternalOccurrence],
        edges_max: usize,
    ) {
        if occurrences.is_empty() && external.is_empty() {
            rift_tracing::info!(
                component = "analysis",
                operation = "EdgeStore::finalize_static_adjacency",
                mode = "no_static_inputs",
                graph_outgoing_lists = self.outgoing.len(),
                graph_incoming_lists = self.incoming.len(),
                graph_drops = self.dropped_edges,
                edges_max = edges_max,
                candidate_sources = 0,
                static_admitted = 0,
                skipped_outgoing_rows = 0,
                skipped_incoming_rows = 0,
                "EdgeStore::finalize_static_adjacency"
            );
            return;
        }
        let graph = GraphAdjacency {
            outgoing_lists: self.outgoing.len(),
            incoming_lists: self.incoming.len(),
            dropped_edges: self.dropped_edges,
            rows: self.outgoing.values().map(Vec::len).sum(),
        };
        let mut remaining = edges_max.saturating_sub(graph.rows);
        let mut admitted = BTreeMap::<&SymbolId, Vec<Edge>>::new();
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
                .or_else(|| spans.resolve(&self.edge_modules, &occurrence.binding))
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
                derivation: if occurrence.edge_kind.is_some() {
                    rift_protocol::read::EdgeDerivation::Syntax
                } else {
                    rift_protocol::read::EdgeDerivation::Resolution
                },
            };
            admitted.entry(from).or_default().push(edge);
        }
        admit_external(external, spans, &self.edge_modules, &mut admitted);
        let candidate_sources = admitted.len();
        let base_keys = (!admitted.is_empty()
            && remaining > 0
            && self.dropped_edges == 0
            && self.outgoing.len().saturating_add(self.incoming.len())
                <= STATIC_FINALIZATION_BASE_KEYS_MAX)
            .then(|| {
                (
                    self.outgoing.keys().cloned().collect(),
                    self.incoming.keys().cloned().collect(),
                )
            });
        for (from, mut edges) in admitted {
            edges.sort_by(edge_order);
            edges.dedup();
            let existing = self.outgoing(from);
            edges.retain(|edge| {
                existing
                    .binary_search_by(|other| edge_order(other, edge))
                    .is_err()
            });
            let fits = edges.len().min(remaining);
            self.dropped_edges = self
                .dropped_edges
                .saturating_add(u64::try_from(edges.len() - fits).unwrap_or(u64::MAX));
            remaining -= fits;
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
        self.finalize_static(&graph, base_keys, edges_max, remaining, candidate_sources);
    }

    fn finalize_static(
        &mut self,
        graph: &GraphAdjacency,
        base_keys: Option<(Vec<SymbolId>, Vec<SymbolId>)>,
        edges_max: usize,
        remaining: usize,
        candidate_sources: usize,
    ) {
        let static_admitted = edges_max.saturating_sub(graph.rows) - remaining;
        let keys = base_keys.filter(|_| remaining > 0 && self.dropped_edges == 0);
        let mode = if static_admitted == 0 {
            "no_new_rows"
        } else if keys.is_some() {
            "static_only"
        } else {
            "full"
        };
        let (
            (outgoing_lists, outgoing_rows, skipped_outgoing_lists, skipped_outgoing_rows),
            (incoming_lists, incoming_rows, skipped_incoming_lists, skipped_incoming_rows),
        ) = if static_admitted == 0 {
            // The ordinary pass leaves even duplicate graph rows alone until a new row fits.
            ((0, 0, 0, 0), (0, 0, 0, 0))
        } else {
            // The static list for each source is sorted and deduplicated. A static-only
            // destination receives its subsequence in ascending source order. Retain
            // ordinary finalization for every original list, including unmodified ones.
            let (outgoing_keys, incoming_keys) = keys.unzip();
            let rows = graph.rows + static_admitted;
            (
                finalize_lists(&mut self.outgoing, outgoing_keys, rows),
                finalize_lists(&mut self.incoming, incoming_keys, rows),
            )
        };
        rift_tracing::info!(
            component = "analysis",
            operation = "EdgeStore::finalize_static_adjacency",
            mode = mode,
            graph_outgoing_lists = graph.outgoing_lists,
            graph_incoming_lists = graph.incoming_lists,
            graph_rows = graph.rows,
            candidate_sources = candidate_sources,
            graph_drops = graph.dropped_edges,
            static_drops = self.dropped_edges - graph.dropped_edges,
            static_admitted = static_admitted,
            edges_max = edges_max,
            remaining = remaining,
            outgoing_lists = outgoing_lists,
            outgoing_rows = outgoing_rows,
            skipped_outgoing_lists = skipped_outgoing_lists,
            skipped_outgoing_rows = skipped_outgoing_rows,
            incoming_lists = incoming_lists,
            incoming_rows = incoming_rows,
            skipped_incoming_lists = skipped_incoming_lists,
            skipped_incoming_rows = skipped_incoming_rows,
            "EdgeStore::finalize_static_adjacency"
        );
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
        Self::build_with_spans(graph, edges_max).0
    }

    /// Builds graph adjacency and adds static edges with the same definition spans.
    pub(crate) fn build_resolving(
        graph: &NormalizedGraph,
        edges_max: usize,
        resolve: impl FnOnce() -> StaticResolution,
    ) -> (Self, StaticResolution) {
        let (mut store, spans) = rift_tracing::debug_span!(
            target: "rift_measure",
            "analysis.graph_admission",
            component = "measurement",
            operation = "analysis.graph_admission",
        )
        .in_scope(|| Self::build_with_spans(graph, edges_max));
        let resolution = resolve();
        rift_tracing::debug_span!(
            target: "rift_measure",
            "analysis.static_admission",
            component = "measurement",
            operation = "analysis.static_admission",
        )
        .in_scope(|| {
            store.add_static(
                &spans,
                &resolution.occurrences,
                &resolution.external,
                edges_max,
            );
        });
        (store, resolution)
    }

    fn build_with_spans(graph: &NormalizedGraph, edges_max: usize) -> (Self, DefinitionSpans) {
        let UnitDefinitions {
            spans,
            edge_modules,
        } = rift_tracing::debug_span!(
            target: "rift_measure",
            "analysis.enclosing_definitions",
            component = "measurement",
            operation = "analysis.enclosing_definitions",
        )
        .in_scope(|| UnitDefinitions::build(graph));
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
    /// the configured edge cap.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.dropped_edges == 0
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

    fn static_call(
        from: &SymbolId,
        to: &SymbolId,
        start: u64,
        end: u64,
    ) -> super::StaticOccurrence {
        super::StaticOccurrence {
            binding: binding("src/lib.rs", start, end),
            target: super::NormalizedTarget::Symbol(to.clone()),
            target_binding: None,
            source_digest: rift_core::FileDigest::of(b"fixture"),
            callee_range: rift_syntax::ByteRange { start, end },
            role: ReferenceRole::Call,
            edge_from: Some(from.clone()),
            edge_kind: None,
        }
    }

    fn finalization_fields(drain: &mut rift_tracing::LogDrain) -> serde_json::Value {
        let records = drain.queued_records();
        let record = records
            .iter()
            .find(|record| record.operation() == "EdgeStore::finalize_static_adjacency")
            .expect("static finalization was recorded");
        serde_json::from_str(record.fields()).expect("finalization counters are JSON")
    }

    fn assert_static_only_counts(counts: &serde_json::Value) {
        assert_eq!(counts["mode"], "static_only");
        assert_eq!(counts["static_drops"], "0");
        assert_eq!(counts["graph_drops"], "0");
        assert_eq!(counts["remaining"], "1");
        assert_eq!(counts["edges_max"], "7");
        assert_eq!(counts["skipped_outgoing_lists"], "2");
        assert_eq!(counts["skipped_outgoing_rows"], "3");
        assert_eq!(counts["skipped_incoming_lists"], "1");
        assert_eq!(counts["skipped_incoming_rows"], "3");
        assert_eq!(counts["outgoing_rows"], "3");
        assert_eq!(counts["incoming_rows"], "3");
    }

    #[test]
    fn static_only_adjacency_matches_full_finalization_in_both_directions() {
        use crate::static_resolution::EdgeCallCoverage;

        let base = fixture_symbol("base");
        let graph_target = fixture_symbol("graph_target");
        let isolated = fixture_symbol("isolated");
        let isolated_target = fixture_symbol("isolated_target");
        let early = fixture_symbol("early");
        let late = fixture_symbol("late");
        let static_target = fixture_symbol("static_target");
        let path = "src/lib.rs";
        let normalized = graph(vec![
            definition("base", base.as_str(), path, (0, 20)),
            definition("graph_target", graph_target.as_str(), path, (20, 40)),
            definition("isolated", isolated.as_str(), path, (40, 60)),
            definition("isolated_target", isolated_target.as_str(), path, (60, 80)),
            reference(
                "base_call",
                binding(path, 5, 8),
                ReferenceRole::Call,
                "graph_target",
            ),
            reference(
                "isolated_call",
                binding(path, 43, 46),
                ReferenceRole::Call,
                "isolated_target",
            ),
        ]);
        let unavailable = source_unit("src/unavailable.rs");
        let resolution = || super::StaticResolution {
            occurrences: vec![
                static_call(&late, &static_target, 90, 94),
                static_call(&late, &static_target, 90, 94),
                static_call(&early, &static_target, 91, 95),
                static_call(&base, &graph_target, 5, 8),
                static_call(&base, &graph_target, 1, 3),
            ],
            external: vec![super::ExternalOccurrence {
                binding: binding(path, 90, 94),
                target: static_target.clone(),
                callee_range: rift_syntax::ByteRange { start: 92, end: 93 },
                role: ReferenceRole::Call,
                edge_from: Some(late.clone()),
                edge_kind: None,
                declared: false,
            }],
            unavailable_sources: 1,
            omitted: std::collections::BTreeMap::from([(EdgeKind::Calls, 1)]),
            edge_calls: std::collections::BTreeMap::from([(
                unavailable.clone(),
                EdgeCallCoverage {
                    language: Language {
                        name: "rust".to_owned(),
                        dialect: None,
                    },
                    unavailable: true,
                    unresolved: vec![rift_syntax::ByteRange { start: 1, end: 2 }],
                },
            )]),
            ..super::StaticResolution::default()
        };
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("trace");
        let (reused, coverage) = EdgeStore::build_resolving(&normalized, 7, resolution);
        drop(recorder);
        let counts = finalization_fields(&mut drain);
        assert_static_only_counts(&counts);
        assert_eq!(coverage.unavailable_sources, 1);
        assert_eq!(coverage.omitted[&EdgeKind::Calls], 1);
        assert!(coverage.edge_calls[&unavailable].unavailable);
        assert_eq!(coverage.edge_calls[&unavailable].unresolved.len(), 1);

        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("trace");
        let (at_cap, _) = EdgeStore::build_resolving(&normalized, 6, resolution);
        drop(recorder);
        let counts = finalization_fields(&mut drain);
        assert_eq!(counts["mode"], "full");
        assert_eq!(counts["remaining"], "0");
        assert_eq!(counts["static_drops"], "0");
        assert_eq!(counts["skipped_outgoing_rows"], "0");
        assert_eq!(counts["skipped_incoming_rows"], "0");
        assert_eq!(reused, at_cap);
        assert_eq!(reused.dropped_edges(), 0);
        assert_eq!(reused.outgoing(&base).len(), 2);
        let incoming = reused.incoming(&static_target);
        assert_eq!(incoming.len(), 3);
        assert_eq!(incoming[0].from(), &early);
        assert_eq!(&incoming[1..], reused.outgoing(&late));
        assert_eq!(incoming[1].static_callee().expect("local call").start, 90);
        assert_eq!(
            incoming[2].static_callee().expect("external call").start,
            92
        );
        let (truncated, _) = EdgeStore::build_resolving(&normalized, 5, resolution);
        assert_eq!(truncated.dropped_edges(), 1);
        assert_eq!(truncated.edges().count(), 5);
    }

    #[test]
    fn first_static_edge_finalizes_old_only_duplicates_and_zero_fit_does_not() {
        let graph_origin = fixture_symbol("graph_origin");
        let graph_target = fixture_symbol("graph_target");
        let normalized = graph(vec![
            definition("origin", graph_origin.as_str(), "src/lib.rs", (0, 20)),
            definition("target", graph_target.as_str(), "src/lib.rs", (20, 40)),
            reference(
                "origin_call",
                binding("src/lib.rs", 5, 8),
                ReferenceRole::Call,
                "target",
            ),
        ]);
        let occurrence = static_call(
            &fixture_symbol("static_origin"),
            &fixture_symbol("static_target"),
            41,
            44,
        );
        for (cap, base_rows, drops) in [(2, 2, 1), (3, 1, 0), (4, 1, 0)] {
            let (mut store, spans) = EdgeStore::build_with_spans(&normalized, cap);
            for edges in [
                store
                    .outgoing
                    .get_mut(&graph_origin)
                    .expect("base outgoing list"),
                store
                    .incoming
                    .get_mut(&graph_target)
                    .expect("base incoming list"),
            ] {
                edges.push(edges[0].clone());
            }
            let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
                .install()
                .expect("trace");
            store.add_static(&spans, std::slice::from_ref(&occurrence), &[], cap);
            drop(recorder);
            let counts = finalization_fields(&mut drain);
            let expected = match cap {
                2 => "no_new_rows",
                3 => "full",
                _ => "static_only",
            };
            assert_eq!(counts["mode"], expected);
            assert_eq!(counts["graph_rows"], "2");
            assert_eq!(counts["static_drops"], drops.to_string());
            assert_eq!(store.outgoing(&graph_origin).len(), base_rows);
            assert_eq!(store.incoming(&graph_target).len(), base_rows);
            assert_eq!(store.dropped_edges(), drops);
        }
    }

    #[test]
    fn empty_static_input_records_zero_admission_without_changing_graph_rows() {
        let normalized = graph(vec![definition(
            "origin",
            fixture_symbol("origin").as_str(),
            "src/lib.rs",
            (0, 20),
        )]);
        let (mut store, spans) = EdgeStore::build_with_spans(&normalized, 4);
        let expected = EdgeStore::build_capped(&normalized, 4);
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("trace");
        store.add_static(&spans, &[], &[], 4);
        drop(recorder);
        let counts = finalization_fields(&mut drain);
        assert_eq!(counts["mode"], "no_static_inputs");
        assert_eq!(counts["static_admitted"], "0");
        assert_eq!(counts["skipped_outgoing_rows"], "0");
        assert_eq!(counts["skipped_incoming_rows"], "0");
        assert_eq!(store, expected);
    }

    #[test]
    fn too_many_base_endpoints_finalize_all_lists() {
        let mut store = EdgeStore {
            outgoing: std::collections::BTreeMap::new(),
            incoming: std::collections::BTreeMap::new(),
            dropped_edges: 0,
            edge_modules: std::collections::BTreeMap::new(),
        };
        let target = fixture_symbol("base_target");
        for index in 0..super::STATIC_FINALIZATION_BASE_KEYS_MAX {
            let source = fixture_symbol(&format!("base_source_{index}"));
            let edge = super::Edge {
                from: source.clone(),
                to: target.clone(),
                kind: EdgeKind::Calls,
                occurrence: binding("src/lib.rs", 5, 8),
                static_callee: None,
                derivation: rift_protocol::read::EdgeDerivation::Resolution,
                from_graph: false,
            };
            store.outgoing.insert(source, vec![edge.clone()]);
            store.incoming.entry(target.clone()).or_default().push(edge);
        }
        store
            .incoming
            .get_mut(&target)
            .expect("base target")
            .sort_by(super::edge_order);
        let (recorder, mut drain) = rift_tracing::ScopedRecorder::builder()
            .install()
            .expect("trace");
        store.add_static(
            &super::DefinitionSpans(std::collections::BTreeMap::new()),
            &[static_call(
                &fixture_symbol("static_origin"),
                &fixture_symbol("static_target"),
                41,
                44,
            )],
            &[],
            super::EDGES_MAX,
        );
        drop(recorder);
        let counts = finalization_fields(&mut drain);
        assert_eq!(counts["mode"], "full");
        assert_eq!(counts["graph_incoming_lists"], "1");
        assert_eq!(
            counts["graph_outgoing_lists"],
            super::STATIC_FINALIZATION_BASE_KEYS_MAX.to_string()
        );
        assert_eq!(counts["static_admitted"], "1");
        assert_eq!(counts["skipped_outgoing_rows"], "0");
        assert_eq!(counts["skipped_incoming_rows"], "0");
        assert_eq!(
            store.edges().count(),
            super::STATIC_FINALIZATION_BASE_KEYS_MAX + 1
        );
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
            let (shared, _) = EdgeStore::build_resolving(&graph, cap, resolution);
            assert_eq!(DEFINITION_BUILDS.get() - before, 1, "cap {cap}");

            let mut separate = EdgeStore::build_capped(&graph, cap);
            let fresh = UnitDefinitions::build(&graph);
            let separate_resolution = resolution();
            separate.add_static(
                &fresh.spans,
                &separate_resolution.occurrences,
                &separate_resolution.external,
                cap,
            );
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
