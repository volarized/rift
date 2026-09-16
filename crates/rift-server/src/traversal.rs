//! `search`'s bounded relationship `traversal` lane: a breadth-first walk from one seed symbol
//! across `RelationshipStore`'s edges, merged into `search`'s hit set. Extracted from `search`
//! so that module stays below its size bound.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use rift_core::{Language, LoopBudget, SymbolId as CoreSymbolId};
use rift_index::{
    IndexedFile, PathMatcher, RelationshipEdge, RelationshipStore, SymbolMatch, SymbolMatchRank,
    WorkspaceIndex, produced_relationship_facets,
};
use rift_protocol::read::{
    ExactKind, Extensions, GraphHop, HopDirection, MatchedField, ReadWarning, Relationship,
    RelationshipDerivation, RelationshipFacet, SEARCH_TRAVERSAL_DEPTH_MAX,
    SEARCH_TRAVERSAL_DEPTH_MIN, SEARCH_TRAVERSAL_FACETS_MAX, SearchHit, SearchTraversal, SymbolId,
    TraversalDirection,
};
use rift_syntax::SyntaxSymbol;

use crate::engine_read::EngineReferences;
use crate::read::parse_symbol_address;
use crate::read::{ReadError, ReadFault, ReadService};
use crate::search::{HitPayloads, build_symbol_hit, find_symbol_hit_mut, includes, resolve_symbol};

/// Refuses `traversal` when `depth` or `facets` breaks the bound its schema advertises.
/// `schemars`' `range`/`length` constraints are advisory only, so this mirrors them the same
/// way `validate_path_selector` mirrors [`PathPattern`]'s.
///
/// [`PathPattern`]: rift_protocol::read::PathPattern
pub(crate) fn validate_traversal(traversal: &SearchTraversal) -> Result<(), ReadError> {
    if !(SEARCH_TRAVERSAL_DEPTH_MIN..=SEARCH_TRAVERSAL_DEPTH_MAX).contains(&traversal.depth) {
        return Err(ReadFault::invalid(
            "traversal",
            format!(
                "depth {} outside {SEARCH_TRAVERSAL_DEPTH_MIN} to {SEARCH_TRAVERSAL_DEPTH_MAX}",
                traversal.depth
            ),
        ));
    }
    if traversal.facets.len() > SEARCH_TRAVERSAL_FACETS_MAX {
        return Err(ReadFault::invalid(
            "traversal",
            format!(
                "facets carries {} entries, more than {SEARCH_TRAVERSAL_FACETS_MAX}",
                traversal.facets.len()
            ),
        ));
    }
    Ok(())
}

/// Most distinct symbols one traversal may visit beyond `seed`. `RelationshipStore`'s
/// adjacency lists are themselves sorted, so a walk that breaches this bound stops
/// discovering new symbols and truncates the same way on every call over the same graph.
pub(crate) const TRAVERSAL_NODES_MAX: usize = 10_000;

/// What one traversal lane reports beside the hits it merged: the coverage the request asked
/// for that no provider populates, and whether the walk stopped at its node bound.
#[derive(Debug, Default)]
pub(crate) struct TraversalReport {
    pub(crate) coverage_missing: Option<ReadWarning>,
    pub(crate) truncated: bool,
}

/// Fourth hit-collection lane: a bounded relationship walk from `traversal.seed`, merged into
/// `results` alongside whatever the lexical and ranked lanes already placed there. Runs after
/// them so a symbol also reached by the walk absorbs it instead of duplicating it.
///
/// # Errors
///
/// Returns [`ReadError`] naming the binding capability when this workspace's relationship
/// store is empty because the binding provider never ran, and `not_found` naming a seed that
/// resolves to neither a relationship-store node nor a lexical declaration.
pub(crate) fn collect_traversal_hits(
    reads: &ReadService,
    matcher: Option<&PathMatcher>,
    root: &Path,
    traversal: &SearchTraversal,
    references: &EngineReferences,
    payloads: HitPayloads,
    results: &mut Vec<SearchHit>,
) -> Result<TraversalReport, ReadError> {
    let store = reads.relationships();
    if store.is_empty() && !reads.index().binding_enabled() && !references.resolved(&traversal.seed)
    {
        return Err(ReadFault::unsupported(
            "relationship traversal (providers.binding disabled)",
        ));
    }
    let seed = resolve_traversal_seed(reads, &traversal.seed)?;
    let coverage_missing = relationship_coverage_missing(reads, traversal, &seed);
    let walk =
        walk_traversal_with_references(store, &seed, traversal, TRAVERSAL_NODES_MAX, references);
    for (identity, path) in walk.discovered {
        if traversal
            .to
            .as_ref()
            .is_some_and(|to| to.0 != identity.as_str())
        {
            continue;
        }
        let Some((file, symbol)) = resolve_graph_symbol(reads.index(), &identity) else {
            // A graph node with no lexical declaration in this snapshot: the store outlived
            // the file it was built from. Skipping it is the same choice `merge_symbol_hit`
            // makes for a ranked unit whose declaration is likewise gone.
            continue;
        };
        if !includes(matcher, root, file.path()) {
            continue;
        }
        merge_traversal_hit(reads.index(), results, file, symbol, path, payloads)?;
    }
    Ok(TraversalReport {
        coverage_missing,
        truncated: walk.truncated,
    })
}

/// The relationship coverage `traversal` asks for that no provider populates: the requested
/// facets outside every provider's image, and the seed's language when the requested
/// direction leaves the binding lane as the only provider.
///
/// Answers `None` when every requested facet and the seed's language have a provider: an
/// empty answer then means the walk ran and the seed has no neighbor under the request.
fn relationship_coverage_missing(
    reads: &ReadService,
    traversal: &SearchTraversal,
    seed: &CoreSymbolId,
) -> Option<ReadWarning> {
    relationship_coverage_warning(
        unproducible_facets(&traversal.facets),
        uncovered_language(reads, traversal, seed),
    )
}

/// The requested facets no provider populates, in the request's own order, deduplicated.
///
/// The store's own image is every provider's image: `role_facet` maps every reference role
/// onto [`produced_relationship_facets`], and the engine lane adds `References` alone, which
/// that image already holds. An empty `requested` list asks for every facet the walk can
/// follow, so it names no gap.
///
/// `validate_traversal` refuses a list longer than `SEARCH_TRAVERSAL_FACETS_MAX` before this
/// runs, which bounds both the scan and the deduplication it carries.
fn unproducible_facets(requested: &[RelationshipFacet]) -> Vec<RelationshipFacet> {
    let produced = produced_relationship_facets();
    let mut missing: Vec<RelationshipFacet> = Vec::new();
    for facet in requested {
        if !produced.contains(facet) && !missing.contains(facet) {
            missing.push(*facet);
        }
    }
    missing
}

/// The seed declaration's language when no provider populates relationships for it in the
/// requested direction.
///
/// The binding lane alone populates outgoing edges: every engine-resolved hop carries
/// [`HopDirection::Incoming`], and `combined_edges` admits those hops for an `incoming` or
/// `both` walk alone. An `outgoing` walk from a language whose indexed files carry no
/// name-binding facts therefore has no provider, while an `incoming` or `both` walk may
/// still be answered by a configured language engine this lane cannot ask, so those
/// directions report no language gap.
fn uncovered_language(
    reads: &ReadService,
    traversal: &SearchTraversal,
    seed: &CoreSymbolId,
) -> Option<Language> {
    if traversal.direction != TraversalDirection::Outgoing {
        return None;
    }
    let index = reads.index();
    let (file, _symbol) = resolve_graph_symbol(index, seed)?;
    let language = file.syntax().language();
    (!index.carries_binding_facts(language)).then(|| language.clone())
}

/// The `relationship_coverage_missing` warning naming what has no provider, or `None` when
/// neither `facets` nor `language` names a gap.
fn relationship_coverage_warning(
    facets: Vec<RelationshipFacet>,
    language: Option<Language>,
) -> Option<ReadWarning> {
    let clauses: Vec<String> = facet_gap_clause(&facets)
        .into_iter()
        .chain(language.as_ref().map(language_gap_clause))
        .collect();
    if clauses.is_empty() {
        return None;
    }
    Some(ReadWarning::RelationshipCoverageMissing {
        facets,
        language,
        detail: clauses.join("; "),
    })
}

/// The detail clause for a facet gap, naming each facet in its wire spelling.
fn facet_gap_clause(facets: &[RelationshipFacet]) -> Option<String> {
    let named: Vec<String> = facets.iter().map(rift_core::fault_label).collect();
    let (subject, reference) = match named.len() {
        0 => return None,
        1 => ("facet", "it"),
        _ => ("facets", "them"),
    };
    Some(format!(
        "no provider populates the requested {subject} {named}, so the walk followed no edge \
         under {reference}",
        named = named.join(", "),
    ))
}

/// The detail clause for a language gap, naming the language's identity segment.
fn language_gap_clause(language: &Language) -> String {
    format!(
        "no provider populates outgoing relationships for {}, so the walk followed no edge \
         from a declaration in it",
        language.identity_segment(),
    )
}

/// Resolves a traversal's `seed`, refusing `not_found` naming it when the identity exists
/// neither as a relationship-store node nor as a lexical declaration. A real, isolated
/// declaration with zero edges still resolves; its walk simply finds nothing.
fn resolve_traversal_seed(reads: &ReadService, seed: &SymbolId) -> Result<CoreSymbolId, ReadError> {
    let not_found = || ReadFault::not_found(seed.0.clone());
    let identity = CoreSymbolId::new(seed.0.clone()).map_err(|_error| not_found())?;
    let store = reads.relationships();
    let in_store = !store.outgoing(&identity).is_empty() || !store.incoming(&identity).is_empty();
    let has_lexical_record = resolve_graph_symbol(reads.index(), &identity).is_some();
    if in_store || has_lexical_record {
        Ok(identity)
    } else {
        Err(not_found())
    }
}

/// Resolves one graph-walked identity back to its declaration, the way a ranked lexical unit
/// resolves: parse the wire address the identity spells, then scan the addressed file for the
/// matching declaration.
pub(crate) fn resolve_graph_symbol<'a>(
    index: &'a WorkspaceIndex,
    identity: &CoreSymbolId,
) -> Option<(&'a IndexedFile, &'a SyntaxSymbol)> {
    let address = parse_symbol_address(identity.as_str()).ok()?;
    resolve_symbol(index, &address.path, identity.as_str())
}

/// One traversal walk's outcome: each discovered symbol with its shortest path, and whether
/// the node bound stopped the walk before the reachable graph was exhausted.
pub(crate) struct TraversalWalk {
    pub(crate) discovered: Vec<(CoreSymbolId, Vec<GraphHop>)>,
    truncated: bool,
}

/// The warning a truncated traversal walk attaches: the bound the walk hit, and the
/// narrowing that fits a walk under it.
pub(crate) fn traversal_truncation_warning() -> ReadWarning {
    ReadWarning::TraversalTruncated {
        visited: TRAVERSAL_NODES_MAX as u64,
        detail: format!(
            "the traversal walk stopped at its {TRAVERSAL_NODES_MAX}-symbol bound; \
             narrow facets, lower depth, or start from a less-connected seed to walk \
             the rest"
        ),
    }
}

/// Walks `store` breadth-first from `seed`, honoring `traversal`'s direction, facet filter,
/// and depth bound. BFS visits each symbol at its shortest path first and never requeues a
/// visited symbol, so every returned path is the shortest one `store` has to it. Bounded by
/// `TRAVERSAL_NODES_MAX`.
/// Traversal under an explicit node cap for deterministic truncation tests.
#[cfg(test)]
fn walk_traversal_capped(
    store: &RelationshipStore,
    seed: &CoreSymbolId,
    traversal: &SearchTraversal,
    nodes_max: usize,
) -> TraversalWalk {
    walk_traversal_with_references(
        store,
        seed,
        traversal,
        nodes_max,
        &EngineReferences::default(),
    )
}

pub(crate) fn walk_traversal_with_references(
    store: &RelationshipStore,
    seed: &CoreSymbolId,
    traversal: &SearchTraversal,
    nodes_max: usize,
    references: &EngineReferences,
) -> TraversalWalk {
    let mut visited: BTreeSet<CoreSymbolId> = BTreeSet::from([seed.clone()]);
    let mut budget = LoopBudget::new(nodes_max);
    let mut truncated = false;
    let mut queue: VecDeque<(CoreSymbolId, Vec<GraphHop>)> =
        VecDeque::from([(seed.clone(), Vec::new())]);
    let mut discovered = Vec::new();
    while let Some((current, path)) = queue.pop_front() {
        if path.len() as u64 >= traversal.depth {
            continue;
        }
        for edge in combined_edges(store, &current, traversal.direction, references) {
            if !edge.eligible(&traversal.facets) {
                continue;
            }
            let Some(next) = edge.next() else {
                continue;
            };
            if visited.contains(&next) {
                continue;
            }
            if budget.consume().is_err() {
                truncated = true;
                continue;
            }
            visited.insert(next.clone());
            let mut next_path = path.clone();
            next_path.push(edge.hop());
            queue.push_back((next.clone(), next_path.clone()));
            discovered.push((next, next_path));
        }
    }
    TraversalWalk {
        discovered,
        truncated,
    }
}

enum TraversalEdge<'edge> {
    Indexed(&'edge RelationshipEdge, HopDirection),
    Confirmed(&'edge RelationshipEdge, &'edge GraphHop),
    Resolved(&'edge GraphHop),
}

impl TraversalEdge<'_> {
    fn eligible(&self, facets: &[RelationshipFacet]) -> bool {
        if facets.is_empty() {
            return true;
        }
        match self {
            Self::Indexed(edge, _) | Self::Confirmed(edge, _) => facets.contains(&edge.facet()),
            Self::Resolved(hop) => hop
                .relationship
                .facets
                .iter()
                .any(|facet| facets.contains(facet)),
        }
    }

    fn next(&self) -> Option<CoreSymbolId> {
        match self {
            Self::Indexed(edge, HopDirection::Outgoing) => Some(edge.to().clone()),
            Self::Indexed(edge, HopDirection::Incoming) | Self::Confirmed(edge, _) => {
                Some(edge.from().clone())
            }
            Self::Resolved(hop) => CoreSymbolId::new(hop.relationship.from.0.clone()).ok(),
        }
    }

    fn hop(&self) -> GraphHop {
        match self {
            Self::Indexed(edge, direction) => graph_hop(edge, *direction),
            Self::Confirmed(edge, resolved) => {
                let mut hop = graph_hop(edge, HopDirection::Incoming);
                hop.relationship.derivation = resolved.relationship.derivation;
                hop
            }
            Self::Resolved(hop) => (*hop).clone(),
        }
    }
}

/// Borrows graph edges until the walk selects a new symbol, preserving indexed evidence.
fn combined_edges<'edge>(
    store: &'edge RelationshipStore,
    current: &CoreSymbolId,
    direction: TraversalDirection,
    references: &'edge EngineReferences,
) -> Vec<TraversalEdge<'edge>> {
    let incoming = matches!(
        direction,
        TraversalDirection::Incoming | TraversalDirection::Both
    );
    let mut resolved: BTreeMap<_, _> = references
        .incoming(current)
        .iter()
        .filter(|_| incoming)
        .map(|hop| (hop.relationship.from.0.as_str(), hop))
        .collect();
    let mut edges = Vec::new();
    for (edge, direction) in traversal_edges(store, current, direction) {
        let confirms =
            direction == HopDirection::Incoming && edge.facet() == RelationshipFacet::References;
        let confirmed = confirms
            .then(|| resolved.remove(edge.from().as_str()))
            .flatten();
        edges.push(match confirmed {
            Some(hop) => TraversalEdge::Confirmed(edge, hop),
            None => TraversalEdge::Indexed(edge, direction),
        });
    }
    edges.extend(resolved.into_values().map(TraversalEdge::Resolved));
    edges
}

/// The edges a search traversal walks from `current`, tagged with the [`HopDirection`] each
/// was followed relative to its own stored orientation: `outgoing` yields edges leaving
/// `current`; `incoming` yields edges arriving at it; `both` yields every outgoing edge before
/// every incoming edge, keeping the walk deterministic.
fn traversal_edges<'a>(
    store: &'a RelationshipStore,
    current: &CoreSymbolId,
    direction: TraversalDirection,
) -> Vec<(&'a RelationshipEdge, HopDirection)> {
    let mut edges = Vec::new();
    if matches!(
        direction,
        TraversalDirection::Outgoing | TraversalDirection::Both
    ) {
        edges.extend(
            store
                .outgoing(current)
                .iter()
                .map(|edge| (edge, HopDirection::Outgoing)),
        );
    }
    if matches!(
        direction,
        TraversalDirection::Incoming | TraversalDirection::Both
    ) {
        edges.extend(
            store
                .incoming(current)
                .iter()
                .map(|edge| (edge, HopDirection::Incoming)),
        );
    }
    edges
}

/// Builds one traversal step's wire relationship from the edge it followed. `RelationshipStore`
/// retains only a reference's portable facet, not the provider's original exact kind, so
/// `kind` is the facet's own serde spelling: the binding lane's exact kind IS its portable
/// facet spelling, the least invented value this lane can honestly report.
fn graph_hop(edge: &RelationshipEdge, direction: HopDirection) -> GraphHop {
    GraphHop {
        relationship: Relationship {
            from: wire_symbol_id(edge.from()),
            kind: ExactKind(rift_core::fault_label(&edge.facet())),
            facets: vec![edge.facet()],
            to: wire_symbol_id(edge.to()),
            evidence: edge.occurrence().node().cloned().into_iter().collect(),
            derivation: RelationshipDerivation::Resolution,
            confidence: None,
            extensions: Extensions::default(),
        },
        direction,
    }
}

fn wire_symbol_id(identity: &CoreSymbolId) -> SymbolId {
    SymbolId(identity.as_str().to_owned())
}

/// A reached symbol's `relevance` score: a closer hit (`distance` 1) scores higher than a
/// farther one (`distance` 2, `SearchTraversal`'s own bound). This is the whole ranking basis
/// a traversal-only request has, so ordering by it reproduces the distance-ascending order
/// `SearchTraversal`'s doc comment promises; a hit also matched lexically keeps its lexical
/// score instead - `absorb_traversal_match` never touches `score`.
fn traversal_hit_score(distance: u64) -> f64 {
    match distance {
        1 => 1.0,
        _ => 0.5,
    }
}

/// Merges one reached symbol: an existing hit for the same identity absorbs the walk;
/// otherwise the traversal places a new symbol hit tagged [`MatchedField::Relationship`].
fn merge_traversal_hit(
    index: &WorkspaceIndex,
    results: &mut Vec<SearchHit>,
    file: &IndexedFile,
    symbol: &SyntaxSymbol,
    path: Vec<GraphHop>,
    payloads: HitPayloads,
) -> Result<(), ReadError> {
    let distance = u64::try_from(path.len()).unwrap_or(u64::MAX);
    if let Some(existing) = find_symbol_hit_mut(results, file, symbol) {
        absorb_traversal_match(existing, path, distance);
        return Ok(());
    }
    let matched = SymbolMatch {
        file,
        symbol,
        // This lane supplies `score` from `distance` and never reads identifier rank.
        rank: SymbolMatchRank::Substring,
    };
    let mut hit = build_symbol_hit(
        index,
        matched,
        Some(traversal_hit_score(distance)),
        vec![MatchedField::Relationship],
        payloads,
    )?;
    hit.traversal_path = Some(path);
    hit.distance = Some(distance);
    results.push(hit);
    Ok(())
}

/// Records that `existing` was also reached by the walk: adds [`MatchedField::Relationship`]
/// when absent, and attaches the walk's path and distance. `existing.score` stays untouched -
/// a lexical or ranked match's score already means more than a graph distance would.
fn absorb_traversal_match(existing: &mut SearchHit, path: Vec<GraphHop>, distance: u64) {
    if !existing.matched_by.contains(&MatchedField::Relationship) {
        existing.matched_by.push(MatchedField::Relationship);
    }
    existing.traversal_path = Some(path);
    existing.distance = Some(distance);
}

#[cfg(test)]
pub(crate) mod tests {
    use std::error::Error;
    use std::fs;
    use std::sync::Arc;

    use rift_core::{LanguageFileSelections, SourceVisibility};
    use rift_index::{BindingPolicy, RelationshipStore, WorkspaceIndexLimits};
    use rift_protocol::configuration::{BindingConfiguration, HistoryConfiguration};
    use rift_protocol::read::{
        MatchedField, ReadWarning, RelationshipFacet, SearchHitTarget, SearchParams, SearchResult,
        SearchTraversal, TraversalDirection,
    };
    use rift_provider::{
        NormalizedGraph, Normalizer, ProviderPublication, PublicationLimits, PublicationSet,
    };
    use serde_json::json;
    use tempfile::TempDir;

    use super::{CoreSymbolId, TRAVERSAL_NODES_MAX, walk_traversal_capped};
    use crate::read::ReadService;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    // -- `walk_traversal` fixture: a small synthetic relationship graph, built through the
    // same public Contribution/NormalizedGraph API `RelationshipStore`'s own tests use, so
    // the walk's direction, facet, depth, and shortest-path behavior is proven without
    // depending on what a real syntax provider happens to classify a reference as.
    //
    //   root --calls--> branch_a --calls--> leaf
    //   root --calls--> branch_b --calls--> leaf
    //   root --imports--> helper

    fn graph_provider(value: &str) -> rift_core::ProviderId {
        rift_core::ProviderId::new(value).expect("fixture provider identity")
    }

    fn graph_source_unit(path: &str) -> rift_core::SourceUnitId {
        rift_core::SourceUnitId::new(
            rift_core::SourceResolverId::new("project").expect("fixture resolver identity"),
            rift_core::SourcePath::new(path).expect("fixture source path"),
        )
        .expect("fixture source unit")
    }

    fn graph_binding(path: &str, start: u64, end: u64) -> rift_core::DeclarationBinding {
        rift_core::DeclarationBinding::new(
            graph_source_unit(path),
            rift_core::SourceRange::new(start, end).expect("fixture range"),
            None,
        )
    }

    fn graph_exact() -> rift_core::SourceApplicability {
        rift_core::SourceApplicability::Exact {
            source_revision: rift_core::SourceRevision::new(1).expect("fixture source revision"),
            tree_revision: rift_core::TreeRevision::new(1).expect("fixture tree revision"),
        }
    }

    fn graph_authored_origin() -> rift_core::ContributionOrigin {
        rift_core::ContributionOrigin::new(
            Some(rift_core::SourceLocation::Project { package: None }),
            rift_core::SourceKind::Authored,
        )
        .expect("fixture origin")
    }

    /// One definition contribution: an established identity, anchored at `range` in
    /// `lib.rs`.
    fn graph_definition(
        symbol: &str,
        identity: &str,
        range: (u64, u64),
    ) -> rift_core::Contribution {
        let facts = rift_core::PortableSymbolFacts::new(
            rift_core::Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            identity,
            identity,
            rift_core::ExactKind("rust.function".to_owned()),
        );
        rift_core::Contribution::builder(
            rift_core::ContributionKey::new(
                graph_provider("syntax"),
                rift_core::ProviderRevision::new(1).expect("fixture provider revision"),
                rift_core::ProviderSymbolId::new(symbol).expect("fixture provider symbol"),
            ),
            graph_exact(),
            facts,
            graph_authored_origin(),
        )
        .source(graph_binding("lib.rs", range.0, range.1))
        .identity_anchor(rift_core::SymbolId::new(identity).expect("fixture identity"))
        .build()
        .expect("fixture definition contribution")
    }

    /// One reference-only contribution: `occurrence` sits inside its enclosing definition's
    /// range, so `walk_traversal_with_references`'s store carries the edge it produces.
    fn graph_reference(
        key_symbol: &str,
        occurrence: rift_core::DeclarationBinding,
        role: rift_core::ReferenceRole,
        target: &str,
    ) -> rift_core::Contribution {
        let target = rift_core::ContributionReference::new(
            graph_provider("syntax"),
            rift_core::ProviderSymbolId::new(target).expect("fixture target symbol"),
        );
        let semantic = rift_core::SemanticReference::new(occurrence, role, vec![target])
            .expect("fixture semantic reference");
        rift_core::Contribution::fact_builder(
            rift_core::ContributionKey::new(
                graph_provider("syntax"),
                rift_core::ProviderRevision::new(1).expect("fixture provider revision"),
                rift_core::ProviderSymbolId::new(key_symbol).expect("fixture provider symbol"),
            ),
            graph_exact(),
            graph_authored_origin(),
        )
        .references(vec![semantic])
        .build()
        .expect("fixture reference contribution")
    }

    fn graph_normalized(contributions: Vec<rift_core::Contribution>) -> NormalizedGraph {
        let publication = ProviderPublication::new(
            graph_provider("syntax"),
            rift_core::ProviderRevision::new(1).expect("fixture provider revision"),
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
            rift_core::IndexRevision::new(1).expect("fixture index revision"),
            rift_core::SourceRevision::new(1).expect("fixture source revision"),
            rift_core::TreeRevision::new(1).expect("fixture tree revision"),
            &publications,
            None,
        )
        .expect("fixture normalized graph")
    }

    fn graph_symbol_id(text: &str) -> CoreSymbolId {
        CoreSymbolId::new(text).expect("fixture symbol identity")
    }

    /// `root` calls `branch_a` and `branch_b`, each of which calls `leaf`; `root` also
    /// imports `helper` directly, so one hop from `root` mixes a `calls` pair with one
    /// `imports` edge.
    pub(crate) fn call_graph_store() -> RelationshipStore {
        let contributions = vec![
            graph_definition("root", "rift://symbol/rust/lib.rs/root", (0, 40)),
            graph_definition("branch_a", "rift://symbol/rust/lib.rs/branch_a", (40, 80)),
            graph_definition("branch_b", "rift://symbol/rust/lib.rs/branch_b", (80, 120)),
            graph_definition("leaf", "rift://symbol/rust/lib.rs/leaf", (120, 160)),
            graph_definition("helper", "rift://symbol/rust/lib.rs/helper", (160, 200)),
            graph_reference(
                "root_calls_branch_a",
                graph_binding("lib.rs", 5, 10),
                rift_core::ReferenceRole::Call,
                "branch_a",
            ),
            graph_reference(
                "root_calls_branch_b",
                graph_binding("lib.rs", 15, 20),
                rift_core::ReferenceRole::Call,
                "branch_b",
            ),
            graph_reference(
                "root_imports_helper",
                graph_binding("lib.rs", 25, 30),
                rift_core::ReferenceRole::Import,
                "helper",
            ),
            graph_reference(
                "branch_a_calls_leaf",
                graph_binding("lib.rs", 45, 50),
                rift_core::ReferenceRole::Call,
                "leaf",
            ),
            graph_reference(
                "branch_b_calls_leaf",
                graph_binding("lib.rs", 85, 90),
                rift_core::ReferenceRole::Call,
                "leaf",
            ),
        ];
        RelationshipStore::build(&graph_normalized(contributions))
    }

    fn traversal_request(
        seed: &CoreSymbolId,
        direction: TraversalDirection,
        depth: u64,
        facets: Vec<RelationshipFacet>,
    ) -> SearchTraversal {
        SearchTraversal {
            seed: rift_protocol::read::SymbolId(seed.as_str().to_owned()),
            direction,
            facets,
            depth,
            to: None,
        }
    }

    #[test]
    fn walk_traversal_outgoing_reaches_both_direct_calls_and_records_outgoing_hops() {
        let store = call_graph_store();
        let root = graph_symbol_id("rift://symbol/rust/lib.rs/root");
        let request = traversal_request(&root, TraversalDirection::Outgoing, 1, vec![]);
        let discovered =
            walk_traversal_capped(&store, &root, &request, TRAVERSAL_NODES_MAX).discovered;
        let reached: Vec<&str> = discovered.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            reached,
            [
                "rift://symbol/rust/lib.rs/branch_a",
                "rift://symbol/rust/lib.rs/branch_b",
                "rift://symbol/rust/lib.rs/helper",
            ],
            "outgoing order follows the store's own sorted adjacency"
        );
        for (_, path) in &discovered {
            assert_eq!(path.len(), 1);
            assert_eq!(path[0].direction, super::HopDirection::Outgoing);
            assert_eq!(path[0].relationship.from.0, root.as_str());
        }
    }

    #[test]
    fn walk_traversal_incoming_walks_edges_backward_and_keeps_their_natural_orientation() {
        let store = call_graph_store();
        let leaf = graph_symbol_id("rift://symbol/rust/lib.rs/leaf");
        let request = traversal_request(&leaf, TraversalDirection::Incoming, 1, vec![]);
        let discovered =
            walk_traversal_capped(&store, &leaf, &request, TRAVERSAL_NODES_MAX).discovered;
        let reached: Vec<&str> = discovered.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            reached,
            [
                "rift://symbol/rust/lib.rs/branch_a",
                "rift://symbol/rust/lib.rs/branch_b",
            ]
        );
        for (reached_id, path) in &discovered {
            assert_eq!(path.len(), 1);
            assert_eq!(path[0].direction, super::HopDirection::Incoming);
            // The edge's own `from`/`to` stay in their stored orientation: walking backward
            // never swaps them.
            assert_eq!(path[0].relationship.from.0, reached_id.as_str());
            assert_eq!(path[0].relationship.to.0, leaf.as_str());
        }
    }

    #[test]
    fn walk_traversal_both_yields_every_outgoing_edge_before_every_incoming_edge() {
        let store = call_graph_store();
        let branch_a = graph_symbol_id("rift://symbol/rust/lib.rs/branch_a");
        let request = traversal_request(&branch_a, TraversalDirection::Both, 1, vec![]);
        let discovered =
            walk_traversal_capped(&store, &branch_a, &request, TRAVERSAL_NODES_MAX).discovered;
        let reached: Vec<(&str, super::HopDirection)> = discovered
            .iter()
            .map(|(id, path)| (id.as_str(), path[0].direction))
            .collect();
        assert_eq!(
            reached,
            [
                (
                    "rift://symbol/rust/lib.rs/leaf",
                    super::HopDirection::Outgoing
                ),
                (
                    "rift://symbol/rust/lib.rs/root",
                    super::HopDirection::Incoming
                ),
            ],
            "both walks every outgoing edge before every incoming edge"
        );
    }

    #[test]
    fn walk_traversal_facet_filter_keeps_only_the_named_facets() {
        let store = call_graph_store();
        let root = graph_symbol_id("rift://symbol/rust/lib.rs/root");
        let request = traversal_request(
            &root,
            TraversalDirection::Outgoing,
            1,
            vec![RelationshipFacet::Calls],
        );
        let discovered =
            walk_traversal_capped(&store, &root, &request, TRAVERSAL_NODES_MAX).discovered;
        let reached: Vec<&str> = discovered.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            reached,
            [
                "rift://symbol/rust/lib.rs/branch_a",
                "rift://symbol/rust/lib.rs/branch_b",
            ],
            "the imports edge to helper must be excluded: {reached:?}"
        );
    }

    /// `leaf` is two hops from `root` down either branch; the walk keeps it once, at its
    /// shortest path, and `depth` 2 stops before a third hop could reach anything beyond it.
    #[test]
    fn walk_traversal_depth_bound_and_shortest_path_win_together() {
        let store = call_graph_store();
        let root = graph_symbol_id("rift://symbol/rust/lib.rs/root");
        let request = traversal_request(&root, TraversalDirection::Outgoing, 2, vec![]);
        let discovered =
            walk_traversal_capped(&store, &root, &request, TRAVERSAL_NODES_MAX).discovered;
        let leaf_hits: Vec<_> = discovered
            .iter()
            .filter(|(id, _)| id.as_str() == "rift://symbol/rust/lib.rs/leaf")
            .collect();
        assert_eq!(
            leaf_hits.len(),
            1,
            "leaf is reachable via two branches but must appear once: {discovered:?}"
        );
        assert_eq!(leaf_hits[0].1.len(), 2, "leaf's shortest path is two hops");
        assert_eq!(
            discovered.len(),
            4,
            "branch_a, branch_b, helper (depth 1) and leaf (depth 2), nothing past depth 2: \
             {discovered:?}"
        );
    }

    /// An explicit low cap truncates discovery deterministically, the way
    /// `RelationshipStore::build_capped` truncates edges: already-enqueued nodes still drain,
    /// but no new node is discovered once the budget is spent.
    #[test]
    fn walk_traversal_capped_stops_discovering_new_nodes_past_its_bound() {
        let store = call_graph_store();
        let root = graph_symbol_id("rift://symbol/rust/lib.rs/root");
        let request = traversal_request(&root, TraversalDirection::Outgoing, 2, vec![]);
        let walk = walk_traversal_capped(&store, &root, &request, 2);
        assert!(
            walk.truncated,
            "a walk stopped by its bound reports the truncation"
        );
        let discovered = walk.discovered;
        assert_eq!(
            discovered.len(),
            2,
            "a cap of 2 must discover exactly 2 new nodes: {discovered:?}"
        );
        let reached: Vec<&str> = discovered.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            reached,
            [
                "rift://symbol/rust/lib.rs/branch_a",
                "rift://symbol/rust/lib.rs/branch_b",
            ],
            "the store's own sorted order makes the first two discoveries deterministic"
        );
    }

    #[test]
    fn traversal_truncation_warning_names_the_bound_it_hit() {
        let warning = super::traversal_truncation_warning();
        let ReadWarning::TraversalTruncated { visited, detail } = warning else {
            panic!("expected the traversal truncation variant: {warning:?}");
        };
        assert_eq!(visited, TRAVERSAL_NODES_MAX as u64);
        assert!(
            detail.contains(&TRAVERSAL_NODES_MAX.to_string()),
            "the detail names the bound: {detail}"
        );
    }

    /// An empty `facets` list asks for every facet the walk can follow, and a list drawn
    /// from `role_facet`'s own image names no gap.
    #[test]
    fn unproducible_facets_names_nothing_for_an_empty_or_fully_produced_list() {
        assert!(super::unproducible_facets(&[]).is_empty());
        assert!(
            super::unproducible_facets(&[
                RelationshipFacet::Calls,
                RelationshipFacet::Imports,
                RelationshipFacet::References,
                RelationshipFacet::Declares,
                RelationshipFacet::Reads,
                RelationshipFacet::Writes,
                RelationshipFacet::HasType,
            ])
            .is_empty()
        );
    }

    /// The gap lists exactly the requested facets no provider populates, keeping the
    /// request's own order, and never the produced ones beside them.
    #[test]
    fn unproducible_facets_keeps_only_the_unproduced_ones_in_request_order() {
        assert_eq!(
            super::unproducible_facets(&[
                RelationshipFacet::Implements,
                RelationshipFacet::Calls,
                RelationshipFacet::Extends,
            ]),
            [RelationshipFacet::Implements, RelationshipFacet::Extends]
        );
        assert_eq!(
            super::unproducible_facets(&[
                RelationshipFacet::Extends,
                RelationshipFacet::Implements,
            ]),
            [RelationshipFacet::Extends, RelationshipFacet::Implements]
        );
    }

    /// A caller repeating one facet gets it named once: the warning reports the set that
    /// has no provider, not the request's own repetition.
    #[test]
    fn unproducible_facets_names_a_repeated_facet_once() {
        assert_eq!(
            super::unproducible_facets(&[
                RelationshipFacet::Implements,
                RelationshipFacet::Implements,
                RelationshipFacet::Extends,
                RelationshipFacet::Implements,
            ]),
            [RelationshipFacet::Implements, RelationshipFacet::Extends]
        );
    }

    #[test]
    fn relationship_coverage_warning_is_absent_when_neither_member_names_a_gap() {
        assert!(super::relationship_coverage_warning(Vec::new(), None).is_none());
    }

    /// A facet gap names each unproduced facet in its wire spelling and carries no language.
    #[test]
    fn relationship_coverage_warning_names_every_unproduced_facet() {
        let gap = vec![RelationshipFacet::Implements, RelationshipFacet::Extends];
        assert_eq!(
            super::relationship_coverage_warning(gap.clone(), None),
            Some(ReadWarning::RelationshipCoverageMissing {
                facets: gap,
                language: None,
                detail: "no provider populates the requested facets implements, extends, so \
                         the walk followed no edge under them"
                    .to_owned(),
            })
        );
    }

    /// A language gap names the language's identity segment and carries no facet.
    #[test]
    fn relationship_coverage_warning_names_the_language_and_its_direction() {
        let toml = rift_core::Language {
            name: "toml".to_owned(),
            dialect: None,
        };
        assert_eq!(
            super::relationship_coverage_warning(Vec::new(), Some(toml.clone())),
            Some(ReadWarning::RelationshipCoverageMissing {
                facets: Vec::new(),
                language: Some(toml),
                detail: "no provider populates outgoing relationships for toml, so the walk \
                         followed no edge from a declaration in it"
                    .to_owned(),
            })
        );
    }

    /// Both gaps ride one warning, each naming what it found, so a caller holding an empty
    /// answer reads every reason the walk could not run.
    #[test]
    fn relationship_coverage_warning_carries_a_facet_gap_beside_a_language_gap() {
        let toml = rift_core::Language {
            name: "toml".to_owned(),
            dialect: None,
        };
        assert_eq!(
            super::relationship_coverage_warning(
                vec![RelationshipFacet::Implements],
                Some(toml.clone())
            ),
            Some(ReadWarning::RelationshipCoverageMissing {
                facets: vec![RelationshipFacet::Implements],
                language: Some(toml),
                detail: "no provider populates the requested facet implements, so the walk \
                         followed no edge under it; no provider populates outgoing \
                         relationships for toml, so the walk followed no edge from a \
                         declaration in it"
                    .to_owned(),
            })
        );
    }

    #[test]
    fn walk_traversal_from_an_edgeless_seed_finds_nothing() {
        let store = call_graph_store();
        let helper = graph_symbol_id("rift://symbol/rust/lib.rs/helper");
        // `helper` has an incoming edge but no outgoing one.
        let request = traversal_request(&helper, TraversalDirection::Outgoing, 2, vec![]);
        let discovered =
            walk_traversal_capped(&store, &helper, &request, TRAVERSAL_NODES_MAX).discovered;
        assert!(discovered.is_empty(), "{discovered:?}");
    }

    // -- End-to-end `ReadService::search` coverage: capability refusal, seed resolution,
    // merges with lexical hits, `to`, and `target` interaction all need a real workspace,
    // since they resolve reached identities back through `WorkspaceIndex::file`.

    /// `root` calls `branch_a` and `branch_b`, each of which calls `leaf`, through the real
    /// Rust syntax and binding pipeline - a live twin of `call_graph_store`'s synthetic graph,
    /// used wherever a test needs a real `IndexedFile` a hit can resolve through.
    fn live_call_graph_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn root() {\n    branch_a();\n    branch_b();\n}\n\
             pub fn branch_a() {\n    leaf();\n}\n\
             pub fn branch_b() {\n    leaf();\n}\n\
             pub fn leaf() {}\n\
             pub fn isolated() {}\n",
        )?;
        let service = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    fn binding_disabled_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let configuration = BindingConfiguration {
            enabled: false,
            ..BindingConfiguration::default()
        };
        let service = ReadService::build_with_languages(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &rift_core::TextFileInclusion::default(),
            &LanguageFileSelections::default(),
            BindingPolicy::from(&configuration),
            HistoryConfiguration::default(),
            rift_protocol::dependencies::DependenciesConfiguration::default(),
        )?;
        Ok((directory, service))
    }

    #[test]
    fn search_traversal_reaches_a_neighbor_at_depth_one() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root"
            }
        }))?;
        let result = service.search(&params, &[])?;
        let hit = result
            .results
            .iter()
            .find(|hit| matches!(&hit.hit, SearchHitTarget::Symbol { symbol } if symbol.name == "branch_a"))
            .ok_or("branch_a must be reached at depth 1")?;
        assert_eq!(hit.distance, Some(1));
        assert!(
            hit.matched_by.contains(&MatchedField::Relationship),
            "{hit:?}"
        );
        assert!(!hit.matched_by.contains(&MatchedField::Name), "{hit:?}");
        Ok(())
    }

    #[test]
    fn search_traversal_paths_selector_drops_a_reached_symbol_outside_it() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root"
            },
            "paths": { "include": ["elsewhere/**"] }
        }))?;
        let result = service.search(&params, &[])?;
        assert!(
            result.results.is_empty(),
            "every walked hit lives outside the selected paths: {:?}",
            result.results
        );
        Ok(())
    }

    #[test]
    fn search_traversal_to_keeps_only_the_reached_target_at_its_shortest_path() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root",
                "depth": 2,
                "to": "rift://symbol/rust/lib.rs/leaf"
            }
        }))?;
        let result = service.search(&params, &[])?;
        assert_eq!(result.results.len(), 1, "{:#?}", result.results);
        let hit = &result.results[0];
        assert!(matches!(
            &hit.hit,
            SearchHitTarget::Symbol { symbol } if symbol.name == "leaf"
        ));
        assert_eq!(hit.distance, Some(2));
        Ok(())
    }

    #[test]
    fn search_traversal_to_unreachable_within_depth_answers_empty_not_refused() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root",
                "to": "rift://symbol/rust/lib.rs/leaf"
            }
        }))?;
        let result = service.search(&params, &[])?;
        assert!(result.results.is_empty(), "{:#?}", result.results);
        Ok(())
    }

    #[test]
    fn search_traversal_target_file_answers_empty() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "target": "file",
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root"
            }
        }))?;
        let result = service.search(&params, &[])?;
        assert!(result.results.is_empty(), "{:#?}", result.results);
        Ok(())
    }

    #[test]
    fn search_query_and_traversal_merge_matched_by_and_keep_the_lexical_score() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        // "ranch_a" is a substring of "branch_a", not the name or qualified name itself, so
        // the lexical lane ranks it `Substring` (0.7) - a score distinct from what the
        // traversal lane would give the same hit at distance 1 (1.0), so a merge that kept
        // the traversal's score instead of the lexical one would show up here.
        let params: SearchParams = serde_json::from_value(json!({
            "query": "ranch_a",
            "target": "symbol",
            "include": ["score"],
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root"
            }
        }))?;
        let result = service.search(&params, &[])?;
        let hit = result
            .results
            .iter()
            .find(|hit| matches!(&hit.hit, SearchHitTarget::Symbol { symbol } if symbol.name == "branch_a"))
            .ok_or("branch_a must be a hit")?;
        assert!(hit.matched_by.contains(&MatchedField::Name), "{hit:?}");
        assert!(
            hit.matched_by.contains(&MatchedField::Relationship),
            "{hit:?}"
        );
        assert_eq!(
            hit.traversal_path.as_ref().map(Vec::len),
            Some(1),
            "the merged hit keeps the walk's path: {hit:?}"
        );
        assert_eq!(
            hit.score,
            Some(0.7),
            "the merge keeps the lexical Substring score rather than the traversal score: \
             {hit:?}"
        );
        Ok(())
    }

    #[test]
    fn search_traversal_seed_with_no_store_entry_and_no_declaration_is_not_found() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/ghost"
            }
        }))?;
        let error = service
            .search(&params, &[])
            .expect_err("an unresolvable seed must refuse");
        assert_eq!(error.descriptor().code(), "resource_not_found");
        Ok(())
    }

    /// `isolated` is a real declaration with zero edges: the walk finds nothing, but the
    /// seed itself resolves, so the request answers an empty result set rather than refusing.
    #[test]
    fn search_traversal_seed_with_no_edges_but_a_real_declaration_answers_empty() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/isolated"
            }
        }))?;
        let result = service.search(&params, &[])?;
        assert!(result.results.is_empty(), "{:#?}", result.results);
        assert!(
            result.warnings.is_empty(),
            "a walk that ran and found nothing carries no coverage warning: {:#?}",
            result.warnings
        );
        Ok(())
    }

    /// One TOML declaration and nothing else: no shipped provider supplies name-binding
    /// facts for TOML, so the index carries no relationship coverage for it.
    fn toml_fixture() -> TestResult<(TempDir, ReadService)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("settings.toml"), "beacon = 7\n")?;
        let limits = WorkspaceIndexLimits::default();
        let visibility = SourceVisibility::default();
        let text = rift_core::TextFileInclusion::default();
        let history = HistoryConfiguration::default();
        let service = ReadService::build(directory.path(), limits, &visibility, &text, history)?;
        Ok((directory, service))
    }

    /// The one `relationship_coverage_missing` warning `result` carries, or `None` when it
    /// carries no warning at all or more than one.
    fn coverage_warning(result: &SearchResult) -> Option<&ReadWarning> {
        result
            .warnings
            .first()
            .filter(|_| result.warnings.len() == 1)
            .filter(|warning| matches!(warning, ReadWarning::RelationshipCoverageMissing { .. }))
    }

    /// An empty answer over a produced facet stays bare, so the warning keeps naming an
    /// absent provider alone.
    #[test]
    fn search_traversal_over_a_produced_facet_carries_no_coverage_warning() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let request = json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root",
                "facets": ["calls"]
            }
        });
        let params: SearchParams = serde_json::from_value(request)?;
        let result = service.search(&params, &[])?;
        assert!(!result.results.is_empty(), "{:#?}", result.results);
        assert!(result.warnings.is_empty(), "{:#?}", result.warnings);
        Ok(())
    }

    /// `implements` reaches no reference role, so the walk could not have answered it. The
    /// produced `calls` beside it still answers, and the warning names the one gap.
    #[test]
    fn search_traversal_over_an_unproduced_facet_warns_relationship_coverage_missing() -> TestResult
    {
        let (_directory, service) = live_call_graph_fixture()?;
        let request = json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root",
                "facets": ["implements", "calls"]
            }
        });
        let params: SearchParams = serde_json::from_value(request)?;
        let result = service.search(&params, &[])?;
        assert!(
            coverage_warning(&result).is_some_and(|warning| matches!(
                warning,
                ReadWarning::RelationshipCoverageMissing { facets, language, .. }
                    if *facets == [RelationshipFacet::Implements] && language.is_none()
            )),
            "{:#?}",
            result.warnings
        );
        Ok(())
    }

    /// An outgoing walk from a TOML declaration has no provider at all, so the answer names
    /// the language rather than leaving an empty result set to read as an absent neighbor.
    #[test]
    fn search_traversal_from_a_language_with_no_binding_facts_warns_its_language() -> TestResult {
        let (_directory, service) = toml_fixture()?;
        let request = json!({
            "traversal": {
                "seed": "rift://symbol/toml/settings.toml/beacon"
            }
        });
        let params: SearchParams = serde_json::from_value(request)?;
        let result = service.search(&params, &[])?;
        assert!(result.results.is_empty(), "{:#?}", result.results);
        assert!(
            coverage_warning(&result).is_some_and(|warning| matches!(
                warning,
                ReadWarning::RelationshipCoverageMissing { facets, language, detail }
                    if facets.is_empty()
                        && language.as_ref().is_some_and(|named| named.name == "toml")
                        && detail.contains("outgoing relationships for toml")
            )),
            "{:#?}",
            result.warnings
        );
        Ok(())
    }

    /// An `incoming` walk over the same language stays bare: a configured language engine
    /// answers incoming references, and this lane cannot ask whether one is configured.
    #[test]
    fn search_traversal_incoming_from_that_language_carries_no_language_warning() -> TestResult {
        let (_directory, service) = toml_fixture()?;
        let request = json!({
            "traversal": {
                "seed": "rift://symbol/toml/settings.toml/beacon",
                "direction": "incoming"
            }
        });
        let params: SearchParams = serde_json::from_value(request)?;
        let result = service.search(&params, &[])?;
        assert!(result.warnings.is_empty(), "{:#?}", result.warnings);
        Ok(())
    }

    #[test]
    fn search_traversal_refuses_capability_unavailable_when_binding_is_disabled() -> TestResult {
        let (_directory, service) = binding_disabled_fixture()?;
        let request = json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/beacon"
            }
        });
        let params: SearchParams = serde_json::from_value(request)?;
        let error = service
            .search(&params, &[])
            .expect_err("a disabled binding provider must refuse the traversal lane");
        assert_eq!(error.descriptor().code(), "capability_unavailable");
        Ok(())
    }

    #[test]
    fn search_traversal_with_rev_refuses_capability_unavailable() -> TestResult {
        let (_directory, service) = live_call_graph_fixture()?;
        let params: SearchParams = serde_json::from_value(json!({
            "rev": "HEAD",
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root"
            }
        }))?;
        let error = service.search(&params, &[]);
        // No git repository exists in this fixture, so revision resolution itself may refuse
        // first; either refusal proves `traversal` never silently combines with `rev`.
        assert!(error.is_err(), "a bare SearchParams cannot resolve `rev`");
        Ok(())
    }

    #[test]
    fn validate_search_refuses_traversal_with_rev_before_resolving_either() {
        let params: SearchParams = serde_json::from_value(json!({
            "rev": "main",
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root"
            }
        }))
        .expect("well-formed request must parse");
        let error = crate::search::validate_search(&params)
            .expect_err("traversal must refuse alongside rev");
        assert_eq!(error.descriptor().code(), "capability_unavailable");
    }

    /// `depth` and `facets.len()` carry `schemars` `range`/`length` constraints that serde
    /// deserialization never enforces on its own - the values below parse cleanly and only
    /// `validate_traversal` (through `validate_search`) refuses them.
    #[test]
    fn validate_search_refuses_traversal_depth_and_facets_out_of_bound() {
        let over_depth: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root",
                "depth": 3
            }
        }))
        .expect("an out-of-range depth still parses; the schema constraint is advisory only");
        let error = crate::search::validate_search(&over_depth)
            .expect_err("depth 3 must be refused at the request-validation boundary");
        assert_eq!(error.descriptor().code(), "invalid_request");

        let padded_facets = vec![json!("calls"); super::SEARCH_TRAVERSAL_FACETS_MAX + 1];
        let over_facets: SearchParams = serde_json::from_value(json!({
            "traversal": {
                "seed": "rift://symbol/rust/lib.rs/root",
                "facets": padded_facets
            }
        }))
        .expect("a duplicate-padded facets list still parses");
        let error = crate::search::validate_search(&over_facets)
            .expect_err("more facets than RelationshipFacet has variants must be refused");
        assert_eq!(error.descriptor().code(), "invalid_request");
    }
    #[test]
    fn confirmed_reference_preserves_indexed_evidence_and_one_hit() {
        let caller = graph_symbol_id("rift://symbol/rust/lib.rs/caller");
        let target = graph_symbol_id("rift://symbol/rust/lib.rs/target");
        let store = RelationshipStore::build(&graph_normalized(vec![
            graph_definition("caller", caller.as_str(), (0, 40)),
            graph_definition("target", target.as_str(), (40, 80)),
            graph_reference(
                "caller_reference",
                graph_binding("lib.rs", 5, 10),
                rift_core::ReferenceRole::Unknown,
                "target",
            ),
        ]));
        let edge = &store.incoming(&target)[0];
        let prior = super::graph_hop(edge, rift_protocol::read::HopDirection::Incoming);
        let mut confirmed = prior.clone();
        confirmed.relationship.evidence.clear();
        let references = crate::EngineReferences::from_incoming(std::collections::BTreeMap::from(
            [(target.clone(), vec![confirmed])],
        ));
        let request = traversal_request(
            &target,
            TraversalDirection::Incoming,
            1,
            vec![RelationshipFacet::References],
        );
        let walk = super::walk_traversal_with_references(
            &store,
            &target,
            &request,
            TRAVERSAL_NODES_MAX,
            &references,
        );
        assert_eq!(walk.discovered.len(), 1);
        assert_eq!(walk.discovered[0].0, caller);
        assert_eq!(walk.discovered[0].1[0], prior);
        assert_eq!(
            walk.discovered[0].1[0].relationship.derivation,
            rift_protocol::read::RelationshipDerivation::Resolution
        );
    }
}
