//! Bidirectional symbol reference adjacency, derived once from one [`NormalizedGraph`].
//!
//! [`RelationshipStore::build`] walks [`NormalizedGraph::references`] once and emits one
//! [`RelationshipEdge`] per reference target the binding provider resolved to an established
//! symbol (`NormalizedTarget::Symbol`); a target the graph could not resolve
//! (`NormalizedTarget::Contribution`) contributes no edge. `NormalizedGraph::references`
//! carries only resolution the binding provider produced, so every edge's derivation is
//! resolution - that fact lives here rather than on a per-edge field.
//!
//! An edge's `from` is the enclosing definition: the definition record whose contribution
//! range is the smallest range containing the reference occurrence in the same source unit.
//! [`EnclosingDefinitions`] builds that per-unit interval list once, from every
//! `Contribution::source` binding a record's contributions carry, and resolves each
//! occurrence against it. A reference with no enclosing definition in its unit - an import at
//! module scope, for instance - contributes no edge.
//!
//! An edge's `facet` is [`role_facet`]'s mapping of the reference's [`ReferenceRole`].
//!
//! An edge's `occurrence` is the reference's own [`DeclarationBinding`]: unit, byte range, and
//! an optional syntax node. The name-binding publisher and the SCIP adapter both construct
//! reference bindings with `node: None`, so the node arm is populated by no production path.
//!
//! Building stops at [`RELATIONSHIP_EDGES_MAX`] edges; [`RelationshipStore::is_complete`] and
//! [`RelationshipStore::dropped_edges`] report the truncation.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use rift_core::{
    DeclarationBinding, LoopBudget, ReferenceRole, SourceRange, SourceUnitId, SymbolId,
};
use rift_protocol::configuration::BINDING_GRAPH_LINKS_MAX;
use rift_protocol::read::RelationshipFacet;
use rift_provider::{NormalizedGraph, NormalizedReference, NormalizedTarget};

/// Most edges one [`RelationshipStore`] holds across both directions combined.
///
/// The source data is already bounded upstream by `[providers.binding]`'s configured
/// limits: `max_graph_links` advertises [`BINDING_GRAPH_LINKS_MAX`] as its schema
/// ceiling. This cap sits generously above that ceiling so an accepted configuration
/// cannot truncate the store in practice, while still bounding the store's own
/// construction work and memory against a value built outside acceptance.
pub const RELATIONSHIP_EDGES_MAX: usize = 10_000_000;

const _: () = assert!(
    RELATIONSHIP_EDGES_MAX as u64 > BINDING_GRAPH_LINKS_MAX,
    "RELATIONSHIP_EDGES_MAX must stay generously above the configured max_graph_links ceiling"
);

/// One resolved reference edge: the enclosing definition a reference occurred in, the
/// established symbol it resolved to, and the reference's portable category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelationshipEdge {
    from: SymbolId,
    to: SymbolId,
    facet: RelationshipFacet,
    occurrence: DeclarationBinding,
}

impl RelationshipEdge {
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
    pub const fn facet(&self) -> RelationshipFacet {
        self.facet
    }

    /// Where the reference occurred: unit, byte range, and syntax node when the binding
    /// provider supplied one. No production binding supplies a node (module doc).
    #[must_use]
    pub const fn occurrence(&self) -> &DeclarationBinding {
        &self.occurrence
    }
}

/// Per-unit definition ranges, resolving which definition encloses one reference occurrence.
///
/// Built once from every contribution [`NormalizedGraph::records`] holds that carries a
/// `Contribution::source` binding - only a definition's own declaring contribution sets
/// that binding, so this is exactly the set of definitions with a placeable range.
struct EnclosingDefinitions {
    by_unit: BTreeMap<SourceUnitId, Vec<(SourceRange, SymbolId)>>,
}

impl EnclosingDefinitions {
    /// Complexity is proportional to the graph's own record and contribution count, both
    /// already bounded by `[providers.binding]`'s `max_graph_nodes`.
    fn build(graph: &NormalizedGraph) -> Self {
        let mut by_unit: BTreeMap<SourceUnitId, Vec<(SourceRange, SymbolId)>> = BTreeMap::new();
        for record in graph.records() {
            let Some(identity) = record.identity() else {
                continue;
            };
            for key in record.contributions() {
                let Some(contribution) = graph.contribution(key) else {
                    continue;
                };
                let Some(binding) = contribution.source() else {
                    continue;
                };
                by_unit
                    .entry(binding.unit().clone())
                    .or_default()
                    .push((binding.range(), identity.clone()));
            }
        }
        for ranges in by_unit.values_mut() {
            ranges.sort_by_key(|(range, _)| (range.start(), range.end()));
        }
        Self { by_unit }
    }

    /// The innermost definition whose range contains `occurrence`'s range in its unit.
    /// Answers `None` when no definition in that unit encloses it. Two enclosing
    /// candidates of equal size tie toward the earlier-starting one.
    ///
    /// `by_unit` sorts each unit's candidates by start, so every candidate that could
    /// contain `occurrence` sits in the prefix ending where `partition_point` splits it;
    /// candidates starting later cannot contain an occurrence that starts no later than
    /// they do. Bounded by that unit's definition count, itself bounded by
    /// `max_unit_definitions`.
    fn resolve(&self, occurrence: &DeclarationBinding) -> Option<&SymbolId> {
        let candidates = self.by_unit.get(occurrence.unit())?;
        let range = occurrence.range();
        let prefix_end =
            candidates.partition_point(|(candidate, _)| candidate.start() <= range.start());
        candidates[..prefix_end]
            .iter()
            .filter(|(candidate, _)| range.end() <= candidate.end())
            .min_by_key(|(candidate, _)| candidate.end() - candidate.start())
            .map(|(_, identity)| identity)
    }
}

/// Maps one reference's portable [`ReferenceRole`] onto the wire [`RelationshipFacet`] this
/// store's edges carry. `Unknown` - a provider exposing no sharper role - and every role this
/// mapping has no sharper facet for falls back to `References`, the facet's general-mention
/// sense.
const fn role_facet(role: ReferenceRole) -> RelationshipFacet {
    match role {
        ReferenceRole::Definition => RelationshipFacet::Declares,
        ReferenceRole::Read => RelationshipFacet::Reads,
        ReferenceRole::Write => RelationshipFacet::Writes,
        ReferenceRole::Import => RelationshipFacet::Imports,
        ReferenceRole::Call => RelationshipFacet::Calls,
        ReferenceRole::Type => RelationshipFacet::HasType,
        ReferenceRole::Unknown => RelationshipFacet::References,
    }
}

/// The edges one reference contributes: one per [`NormalizedTarget::Symbol`] target. A
/// [`NormalizedTarget::Contribution`] target - the graph never resolved it to an established
/// symbol - contributes no edge.
fn reference_edges<'graph>(
    reference: &'graph NormalizedReference,
    from: &'graph SymbolId,
    facet: RelationshipFacet,
) -> impl Iterator<Item = RelationshipEdge> + 'graph {
    reference.targets().iter().filter_map(move |target| {
        let NormalizedTarget::Symbol(to) = target else {
            return None;
        };
        Some(RelationshipEdge {
            from: from.clone(),
            to: to.clone(),
            facet,
            occurrence: reference.binding().clone(),
        })
    })
}

/// Orders two edges sharing one adjacency-list key by their other endpoint, then facet, then
/// occurrence position - so an outgoing list (constant `from`) orders by `to` and an
/// incoming list (constant `to`) orders by `from`, and two edges to the same neighbor from
/// different call sites still land in a stable order.
fn edge_order(left: &RelationshipEdge, right: &RelationshipEdge) -> Ordering {
    (
        &left.from,
        &left.to,
        left.facet,
        left.occurrence.range().start(),
    )
        .cmp(&(
            &right.from,
            &right.to,
            right.facet,
            right.occurrence.range().start(),
        ))
}

/// Bidirectional symbol reference adjacency for one normalized graph.
///
/// See the module doc for what an edge carries and how `from`, `facet`, and truncation are
/// derived.
#[derive(Debug, PartialEq, Eq)]
pub struct RelationshipStore {
    outgoing: BTreeMap<SymbolId, Vec<RelationshipEdge>>,
    incoming: BTreeMap<SymbolId, Vec<RelationshipEdge>>,
    dropped_edges: u64,
}

impl RelationshipStore {
    /// Builds the adjacency from every reference in `graph`, capped at
    /// [`RELATIONSHIP_EDGES_MAX`].
    #[must_use]
    pub fn build(graph: &NormalizedGraph) -> Self {
        Self::build_capped(graph, RELATIONSHIP_EDGES_MAX)
    }

    /// Builds the adjacency under an explicit edge cap, so a test can force truncation
    /// without a graph sized past [`RELATIONSHIP_EDGES_MAX`].
    fn build_capped(graph: &NormalizedGraph, edges_max: usize) -> Self {
        let enclosing = EnclosingDefinitions::build(graph);
        let mut outgoing: BTreeMap<SymbolId, Vec<RelationshipEdge>> = BTreeMap::new();
        let mut incoming: BTreeMap<SymbolId, Vec<RelationshipEdge>> = BTreeMap::new();
        let mut budget = LoopBudget::new(edges_max);
        let mut dropped_edges: u64 = 0;
        for reference in graph.references() {
            let Some(from) = enclosing.resolve(reference.binding()) else {
                continue;
            };
            let facet = role_facet(reference.role());
            for edge in reference_edges(reference, from, facet) {
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
        for edges in outgoing.values_mut() {
            edges.sort_by(edge_order);
        }
        for edges in incoming.values_mut() {
            edges.sort_by(edge_order);
        }
        Self {
            outgoing,
            incoming,
            dropped_edges,
        }
    }

    /// Edges starting at `from`, in deterministic order. Empty when `from` has none.
    #[must_use]
    pub fn outgoing(&self, from: &SymbolId) -> &[RelationshipEdge] {
        self.outgoing.get(from).map_or(&[], Vec::as_slice)
    }

    /// Edges pointing at `to`, in deterministic order. Empty when `to` has none.
    #[must_use]
    pub fn incoming(&self, to: &SymbolId) -> &[RelationshipEdge] {
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

    use super::{ReferenceRole, RelationshipFacet, RelationshipStore, role_facet};

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
            None,
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

    /// One definition contribution: an established identity, anchored at `range` in `path`.
    fn definition(symbol: &str, identity: &str, path: &str, range: (u64, u64)) -> Contribution {
        let facts = PortableSymbolFacts::new(
            Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            identity,
            identity,
            ExactKind("rust.function".to_owned()),
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
    fn role_facet_maps_every_reference_role() {
        let cases = [
            (ReferenceRole::Definition, RelationshipFacet::Declares),
            (ReferenceRole::Read, RelationshipFacet::Reads),
            (ReferenceRole::Write, RelationshipFacet::Writes),
            (ReferenceRole::Import, RelationshipFacet::Imports),
            (ReferenceRole::Call, RelationshipFacet::Calls),
            (ReferenceRole::Type, RelationshipFacet::HasType),
            (ReferenceRole::Unknown, RelationshipFacet::References),
        ];
        for (role, expected) in cases {
            assert_eq!(
                role_facet(role),
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
        let store = RelationshipStore::build(&graph(contributions));

        let beta = symbol("rift://symbol/rust/src/lib.rs/beta");
        let alpha = symbol("rift://symbol/rust/src/lib.rs/alpha");
        let outgoing = store.outgoing(&beta);
        let incoming = store.incoming(&alpha);
        assert_eq!(outgoing.len(), 1);
        assert_eq!(incoming.len(), 1);
        assert_eq!(outgoing[0], incoming[0]);
        assert_eq!(outgoing[0].from(), &beta);
        assert_eq!(outgoing[0].to(), &alpha);
        assert_eq!(outgoing[0].facet(), RelationshipFacet::Calls);
        assert!(store.is_complete());
        assert_eq!(store.dropped_edges(), 0);
    }

    #[test]
    fn enclosing_definition_resolution_picks_the_innermost_definition() {
        let occurrence = binding("src/lib.rs", 20, 25);
        let contributions = vec![
            definition(
                "outer",
                "rift://symbol/rust/src/lib.rs/outer",
                "src/lib.rs",
                (0, 60),
            ),
            definition(
                "inner",
                "rift://symbol/rust/src/lib.rs/inner",
                "src/lib.rs",
                (10, 40),
            ),
            definition(
                "helper",
                "rift://symbol/rust/src/lib.rs/helper",
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
        let store = RelationshipStore::build(&graph(contributions));

        let outer = symbol("rift://symbol/rust/src/lib.rs/outer");
        let inner = symbol("rift://symbol/rust/src/lib.rs/inner");
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
        let store = RelationshipStore::build(&graph(contributions));

        let alpha = symbol("rift://symbol/rust/src/lib.rs/alpha");
        assert!(store.incoming(&alpha).is_empty());
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
        assert_eq!(
            RelationshipStore::build(&normalized),
            RelationshipStore::build(&normalized)
        );
    }

    #[test]
    fn building_stops_at_the_edge_cap_and_reports_the_drop() {
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
            definition(
                "gamma",
                "rift://symbol/rust/src/lib.rs/gamma",
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
        let store = super::RelationshipStore::build_capped(&graph(contributions), 1);

        assert!(!store.is_complete());
        assert_eq!(store.dropped_edges(), 1);
        let beta = symbol("rift://symbol/rust/src/lib.rs/beta");
        assert_eq!(
            store.outgoing(&beta).len(),
            1,
            "the cap keeps exactly one edge"
        );
    }

    /// A real syntax-and-binding fixture, proving the store end to end: `beta` calling
    /// `alpha` produces one edge, both directions agree, and the store is complete.
    #[test]
    fn a_real_workspace_fixture_yields_a_call_edge() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        std::fs::create_dir(directory.path().join("src"))?;
        std::fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn alpha() {}\npub fn beta() { alpha(); }\n",
        )?;
        let index = crate::WorkspaceIndex::build(
            directory.path(),
            crate::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
        )?;

        let beta = symbol("rift://symbol/rust/src/lib.rs/beta");
        let alpha = symbol("rift://symbol/rust/src/lib.rs/alpha");
        let outgoing = index.relationships().outgoing(&beta);
        let incoming = index.relationships().incoming(&alpha);
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing, incoming);
        assert_eq!(outgoing[0].facet(), RelationshipFacet::Calls);
        assert!(index.relationships().is_complete());
        Ok(())
    }

    #[test]
    fn a_self_call_yields_one_edge_listed_in_both_directions() {
        let occurrence = binding("src/lib.rs", 10, 17);
        let contributions = vec![
            definition(
                "recurse",
                "rift://symbol/rust/src/lib.rs/recurse",
                "src/lib.rs",
                (0, 40),
            ),
            reference("self_ref", occurrence, ReferenceRole::Call, "recurse"),
        ];
        let store = RelationshipStore::build(&graph(contributions));

        let recurse = symbol("rift://symbol/rust/src/lib.rs/recurse");
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
                "rift://symbol/rust/src/lib.rs/caller",
                "src/lib.rs",
                (0, 40),
            ),
            definition(
                "callee",
                "rift://symbol/rust/src/lib.rs/callee",
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
        let store = RelationshipStore::build(&graph(contributions));

        let from_symbol = symbol("rift://symbol/rust/src/lib.rs/caller");
        let to_symbol = symbol("rift://symbol/rust/src/lib.rs/callee");
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
            "edges sharing endpoint and facet order by occurrence position"
        );
    }
}
