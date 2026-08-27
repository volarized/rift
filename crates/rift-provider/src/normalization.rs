use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_core::{
    Contribution, ContributionError, ContributionKey, ContributionReference, EquivalenceEvidence,
    IndexRevision, SourceApplicability, SourceRevision, SymbolId, SymbolRecord, SymbolResolution,
    TreeRevision, symbol_identity,
};

use crate::PublicationSet;

/// Result of one association rule that did not establish identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssociationState {
    /// Provider supplied target only for retrieval or ranking.
    Candidate,
    /// Explicit target is absent from captured publications.
    TargetUnavailable,
    /// Accepted evidence joins distinct established identities.
    Conflicting,
}

/// One retained association that normalization did not accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssociationCandidate {
    source: ContributionReference,
    target: ContributionReference,
    state: AssociationState,
}

impl AssociationCandidate {
    fn new(
        source: ContributionReference,
        target: ContributionReference,
        state: AssociationState,
    ) -> Self {
        Self {
            source,
            target,
            state,
        }
    }

    /// Returns Contribution supplying association.
    #[must_use]
    pub const fn source(&self) -> &ContributionReference {
        &self.source
    }

    /// Returns association target.
    #[must_use]
    pub const fn target(&self) -> &ContributionReference {
        &self.target
    }

    /// Returns retained association state.
    #[must_use]
    pub const fn state(&self) -> AssociationState {
        self.state
    }
}

/// Immutable normalized graph for one captured index revision.
#[derive(Debug)]
pub struct NormalizedGraph {
    index_revision: IndexRevision,
    source_revision: SourceRevision,
    tree_revision: TreeRevision,
    publications: Arc<PublicationSet>,
    records: Vec<SymbolRecord>,
    records_by_contribution: BTreeMap<ContributionReference, usize>,
    candidates: Vec<AssociationCandidate>,
}

impl NormalizedGraph {
    /// Returns captured index revision.
    #[must_use]
    pub const fn index_revision(&self) -> IndexRevision {
        self.index_revision
    }

    /// Returns captured source revision.
    #[must_use]
    pub const fn source_revision(&self) -> SourceRevision {
        self.source_revision
    }

    /// Returns captured tree revision.
    #[must_use]
    pub const fn tree_revision(&self) -> TreeRevision {
        self.tree_revision
    }

    /// Returns immutable provider publications used for normalization.
    #[must_use]
    pub fn publications(&self) -> &Arc<PublicationSet> {
        &self.publications
    }

    /// Returns normalized records in stable Contribution order.
    #[must_use]
    pub fn records(&self) -> &[SymbolRecord] {
        &self.records
    }

    /// Returns retained candidate, unavailable, and conflicting associations.
    #[must_use]
    pub fn candidates(&self) -> &[AssociationCandidate] {
        &self.candidates
    }

    /// Returns record associated with provider-local reference.
    #[must_use]
    pub fn record_for(&self, reference: &ContributionReference) -> Option<&SymbolRecord> {
        self.records_by_contribution
            .get(reference)
            .and_then(|index| self.records.get(*index))
    }

    /// Returns established identity associated with provider-local reference.
    #[must_use]
    pub fn identity_for(&self, reference: &ContributionReference) -> Option<&SymbolId> {
        self.record_for(reference).and_then(SymbolRecord::identity)
    }

    /// Returns captured Contribution by immutable key.
    #[must_use]
    pub fn contribution(&self, key: &ContributionKey) -> Option<&Contribution> {
        self.publications
            .provider(key.reference().provider())
            .and_then(|publication| {
                publication
                    .contributions()
                    .iter()
                    .find(|contribution| contribution.key() == key)
            })
    }
}

/// Deterministic Contribution normalizer.
#[derive(Debug, Default, Clone, Copy)]
pub struct Normalizer;

impl Normalizer {
    /// Normalizes captured publications for one index revision.
    ///
    /// Previous graph supplies provider-local continuity. Only identity
    /// associated with same provider-local reference can cross revisions.
    ///
    /// # Errors
    ///
    /// Returns [`ContributionError`] when record construction detects invalid
    /// normalization output.
    pub fn normalize(
        index_revision: IndexRevision,
        source_revision: SourceRevision,
        tree_revision: TreeRevision,
        publications: &Arc<PublicationSet>,
        previous: Option<&NormalizedGraph>,
    ) -> Result<NormalizedGraph, ContributionError> {
        let graph_publications = Arc::clone(publications);
        let contributions = applicable_contributions(publications, source_revision, tree_revision);
        let references = reference_index(&contributions);
        let mut anchors = contributions
            .iter()
            .map(|contribution| contribution_anchors(contribution, previous))
            .collect::<Vec<_>>();
        let mut candidates = Vec::new();
        let edges = association_edges(&contributions, &references, &mut candidates);
        let mut groups = UnionFind::new(contributions.len());

        union_compatible_edges(&mut groups, &edges, &anchors);
        anchors = component_anchors(&mut groups, &anchors);
        let conflicting = attach_unanchored_components(
            &mut groups,
            &edges,
            &anchors,
            &contributions,
            &mut candidates,
        );

        build_graph(GraphBuild {
            index_revision,
            source_revision,
            tree_revision,
            publications: graph_publications,
            contributions: &contributions,
            anchors: &anchors,
            groups,
            conflicting: &conflicting,
            candidates,
        })
    }
}

fn applicable_contributions(
    publications: &PublicationSet,
    source_revision: SourceRevision,
    tree_revision: TreeRevision,
) -> Vec<&Contribution> {
    publications
        .publications()
        .flat_map(crate::ProviderPublication::contributions)
        .filter(|contribution| {
            contribution
                .applicability()
                .applies_to(source_revision, tree_revision)
        })
        .collect()
}

fn reference_index(contributions: &[&Contribution]) -> BTreeMap<ContributionReference, usize> {
    contributions
        .iter()
        .enumerate()
        .map(|(index, contribution)| (contribution.key().reference().clone(), index))
        .collect()
}

fn contribution_anchors(
    contribution: &Contribution,
    previous: Option<&NormalizedGraph>,
) -> BTreeSet<SymbolId> {
    let mut anchors = BTreeSet::new();
    if let Some(identity) = contribution.identity_anchor() {
        anchors.insert(identity.clone());
    }
    if let Some(identity) =
        previous.and_then(|graph| graph.identity_for(contribution.key().reference()))
    {
        anchors.insert(identity.clone());
    }
    if anchors.is_empty()
        && matches!(
            contribution.applicability(),
            SourceApplicability::Exact { .. }
        )
        && let Some(source) = contribution.source()
    {
        let facts = contribution.facts();
        let identity = symbol_identity(
            &facts.language().identity_segment(),
            source.unit().key().as_str(),
            facts.qualified_name(),
        );
        if let Ok(identity) = SymbolId::new(identity) {
            anchors.insert(identity);
        }
    }
    anchors
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct BindingKey {
    unit: String,
    start: u64,
    end: u64,
    node: Option<String>,
}

fn binding_key(binding: &rift_core::DeclarationBinding) -> BindingKey {
    BindingKey {
        unit: binding.unit().to_string(),
        start: binding.range().start(),
        end: binding.range().end(),
        node: binding.node().map(|node| format!("{node:?}")),
    }
}

fn association_edges(
    contributions: &[&Contribution],
    references: &BTreeMap<ContributionReference, usize>,
    candidates: &mut Vec<AssociationCandidate>,
) -> Vec<(usize, usize)> {
    let mut edges = BTreeSet::new();
    let mut by_binding = BTreeMap::<BindingKey, Vec<usize>>::new();
    for (index, contribution) in contributions.iter().enumerate() {
        if let Some(binding) = contribution.source() {
            by_binding
                .entry(binding_key(binding))
                .or_default()
                .push(index);
        }
    }
    for indexes in by_binding.values() {
        if let Some((&first, rest)) = indexes.split_first() {
            for &index in rest {
                edges.insert(ordered_edge(first, index));
            }
        }
    }

    for (index, contribution) in contributions.iter().enumerate() {
        for evidence in contribution.equivalence() {
            match evidence {
                EquivalenceEvidence::Declaration(binding) => {
                    if let Some(targets) = by_binding.get(&binding_key(binding)) {
                        for &target in targets {
                            if index != target {
                                edges.insert(ordered_edge(index, target));
                            }
                        }
                    }
                }
                EquivalenceEvidence::Explicit(target) => match references.get(target) {
                    Some(&target_index) => {
                        edges.insert(ordered_edge(index, target_index));
                    }
                    None => candidates.push(AssociationCandidate::new(
                        contribution.key().reference().clone(),
                        target.clone(),
                        AssociationState::TargetUnavailable,
                    )),
                },
                EquivalenceEvidence::Candidate(target) => {
                    candidates.push(AssociationCandidate::new(
                        contribution.key().reference().clone(),
                        target.clone(),
                        AssociationState::Candidate,
                    ));
                }
            }
        }
    }
    edges.into_iter().collect()
}

const fn ordered_edge(left: usize, right: usize) -> (usize, usize) {
    if left < right {
        (left, right)
    } else {
        (right, left)
    }
}

fn union_compatible_edges(
    groups: &mut UnionFind,
    edges: &[(usize, usize)],
    anchors: &[BTreeSet<SymbolId>],
) {
    for &(left, right) in edges {
        let left_anchors = &anchors[left];
        let right_anchors = &anchors[right];
        if (left_anchors.is_empty() && right_anchors.is_empty())
            || (!left_anchors.is_empty() && left_anchors == right_anchors)
        {
            groups.union(left, right);
        }
    }
}

fn component_anchors(
    groups: &mut UnionFind,
    anchors: &[BTreeSet<SymbolId>],
) -> Vec<BTreeSet<SymbolId>> {
    let mut combined = vec![BTreeSet::new(); anchors.len()];
    for (index, identities) in anchors.iter().enumerate() {
        let root = groups.find(index);
        combined[root].extend(identities.iter().cloned());
    }
    combined
}

fn attach_unanchored_components(
    groups: &mut UnionFind,
    edges: &[(usize, usize)],
    anchors: &[BTreeSet<SymbolId>],
    contributions: &[&Contribution],
    candidates: &mut Vec<AssociationCandidate>,
) -> BTreeSet<usize> {
    let mut neighbors = BTreeMap::<usize, BTreeMap<SymbolId, usize>>::new();
    for &(left, right) in edges {
        let left_root = groups.find(left);
        let right_root = groups.find(right);
        if left_root == right_root {
            continue;
        }
        let left_anchors = &anchors[left_root];
        let right_anchors = &anchors[right_root];
        match (left_anchors.len(), right_anchors.len()) {
            (0, 1) => {
                let identity = right_anchors.iter().next().expect("one anchor").clone();
                neighbors
                    .entry(left_root)
                    .or_default()
                    .insert(identity, right_root);
            }
            (1, 0) => {
                let identity = left_anchors.iter().next().expect("one anchor").clone();
                neighbors
                    .entry(right_root)
                    .or_default()
                    .insert(identity, left_root);
            }
            _ => {
                retain_conflict(left, right, contributions, candidates);
            }
        }
    }

    let mut conflicting = BTreeSet::new();
    for (root, targets) in neighbors {
        if targets.len() == 1 {
            let target = *targets.values().next().expect("one target");
            groups.union(root, target);
        } else {
            conflicting.insert(root);
            for &(left, right) in edges {
                let left_root = groups.find(left);
                let right_root = groups.find(right);
                if left_root == root || right_root == root {
                    retain_conflict(left, right, contributions, candidates);
                }
            }
        }
    }
    conflicting
}

fn retain_conflict(
    left: usize,
    right: usize,
    contributions: &[&Contribution],
    candidates: &mut Vec<AssociationCandidate>,
) {
    let candidate = AssociationCandidate::new(
        contributions[left].key().reference().clone(),
        contributions[right].key().reference().clone(),
        AssociationState::Conflicting,
    );
    if !candidates.contains(&candidate) {
        candidates.push(candidate);
    }
}

struct GraphBuild<'a> {
    index_revision: IndexRevision,
    source_revision: SourceRevision,
    tree_revision: TreeRevision,
    publications: Arc<PublicationSet>,
    contributions: &'a [&'a Contribution],
    anchors: &'a [BTreeSet<SymbolId>],
    groups: UnionFind,
    conflicting: &'a BTreeSet<usize>,
    candidates: Vec<AssociationCandidate>,
}

fn build_graph(input: GraphBuild<'_>) -> Result<NormalizedGraph, ContributionError> {
    let GraphBuild {
        index_revision,
        source_revision,
        tree_revision,
        publications,
        contributions,
        anchors,
        mut groups,
        conflicting,
        mut candidates,
    } = input;
    let mut members = BTreeMap::<usize, Vec<usize>>::new();
    for index in 0..contributions.len() {
        members.entry(groups.find(index)).or_default().push(index);
    }

    let mut records = Vec::with_capacity(members.len());
    let mut records_by_contribution = BTreeMap::new();
    for (root, indexes) in members {
        let mut identities = BTreeSet::new();
        let mut keys = Vec::with_capacity(indexes.len());
        for &index in &indexes {
            let contribution = contributions[index];
            identities.extend(anchors[index].iter().cloned());
            keys.push(contribution.key().clone());
        }
        keys.sort();
        let is_conflicting = conflicting.contains(&root) || identities.len() > 1;
        let (identity, resolution) = if is_conflicting {
            (None, SymbolResolution::Conflicting)
        } else if let Some(identity) = identities.into_iter().next() {
            (Some(identity), SymbolResolution::Established)
        } else {
            (None, SymbolResolution::Unresolved)
        };
        let record = SymbolRecord::new(index_revision, identity, resolution, keys)?;
        let record_index = records.len();
        for &index in &indexes {
            records_by_contribution
                .insert(contributions[index].key().reference().clone(), record_index);
        }
        records.push(record);
    }
    candidates.sort_by(|left, right| {
        (
            left.source.provider(),
            left.source.symbol(),
            left.target.provider(),
            left.target.symbol(),
        )
            .cmp(&(
                right.source.provider(),
                right.source.symbol(),
                right.target.provider(),
                right.target.symbol(),
            ))
    });

    Ok(NormalizedGraph {
        index_revision,
        source_revision,
        tree_revision,
        publications,
        records,
        records_by_contribution,
        candidates,
    })
}

#[derive(Debug)]
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
            rank: vec![0; len],
        }
    }

    fn find(&mut self, index: usize) -> usize {
        let parent = self.parent[index];
        if parent != index {
            self.parent[index] = self.find(parent);
        }
        self.parent[index]
    }

    fn union(&mut self, left: usize, right: usize) {
        let mut left_root = self.find(left);
        let mut right_root = self.find(right);
        if left_root == right_root {
            return;
        }
        if self.rank[left_root] < self.rank[right_root] {
            std::mem::swap(&mut left_root, &mut right_root);
        }
        self.parent[right_root] = left_root;
        if self.rank[left_root] == self.rank[right_root] {
            self.rank[left_root] = self.rank[left_root].saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rift_core::{
        Contribution, ContributionKey, ContributionOrigin, ContributionReference,
        DeclarationBinding, EquivalenceEvidence, ExactKind, IndexRevision, Language,
        PortableSymbolFacts, ProviderId, ProviderRevision, ProviderSymbolId, SourceApplicability,
        SourceKind, SourceLocation, SourceRange, SourceResolverId, SourceRevision, SourceUnitId,
        SymbolId, SymbolResolution, TreeRevision,
    };

    use super::{AssociationState, NormalizedGraph, Normalizer};
    use crate::{ProviderPublication, PublicationLimits, PublicationSet};

    fn provider(value: &str) -> ProviderId {
        ProviderId::new(value).expect("provider")
    }

    fn provider_revision(value: u64) -> ProviderRevision {
        ProviderRevision::new(value).expect("provider revision")
    }

    fn source_revision(value: u64) -> SourceRevision {
        SourceRevision::new(value).expect("source revision")
    }

    fn tree_revision(value: u64) -> TreeRevision {
        TreeRevision::new(value).expect("tree revision")
    }

    fn index_revision(value: u64) -> IndexRevision {
        IndexRevision::new(value).expect("index revision")
    }

    fn source_unit(path: &str) -> SourceUnitId {
        SourceUnitId::new(
            SourceResolverId::new("project").expect("resolver"),
            rift_core::SourcePath::new(path).expect("source path"),
        )
        .expect("source unit")
    }

    fn binding(path: &str, start: u64) -> DeclarationBinding {
        DeclarationBinding::new(
            source_unit(path),
            SourceRange::new(start, start + 8).expect("range"),
            None,
        )
    }

    fn facts(name: &str) -> PortableSymbolFacts {
        PortableSymbolFacts::new(
            Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            name,
            name,
            ExactKind("rust.struct".to_owned()),
        )
    }

    fn reference(provider_id: &str, symbol: &str) -> ContributionReference {
        ContributionReference::new(
            provider(provider_id),
            ProviderSymbolId::new(symbol).expect("provider symbol"),
        )
    }

    struct ContributionInput<'a> {
        provider: &'a str,
        publication: u64,
        symbol: &'a str,
        applicability: SourceApplicability,
        source: Option<DeclarationBinding>,
        identity: Option<&'a str>,
        equivalence: Vec<EquivalenceEvidence>,
    }

    fn contribution(input: ContributionInput<'_>) -> Contribution {
        let origin = if input.source.is_some() {
            ContributionOrigin::new(
                Some(SourceLocation::Project { package: None }),
                SourceKind::Authored,
            )
            .expect("authored origin")
        } else {
            ContributionOrigin::new(None, SourceKind::Synthetic).expect("synthetic origin")
        };
        let mut builder = Contribution::builder(
            ContributionKey::new(
                provider(input.provider),
                provider_revision(input.publication),
                ProviderSymbolId::new(input.symbol).expect("provider symbol"),
            ),
            input.applicability,
            facts(input.symbol),
            origin,
        )
        .equivalence(input.equivalence);
        if let Some(source) = input.source {
            builder = builder.source(source);
        }
        if let Some(identity) = input.identity {
            builder = builder.identity_anchor(SymbolId::new(identity).expect("symbol identity"));
        }
        builder.build().expect("Contribution")
    }

    fn exact(
        provider_id: &str,
        publication: u64,
        symbol: &str,
        tree: u64,
        source: DeclarationBinding,
        identity: Option<&str>,
        equivalence: Vec<EquivalenceEvidence>,
    ) -> Contribution {
        contribution(ContributionInput {
            provider: provider_id,
            publication,
            symbol,
            applicability: SourceApplicability::Exact {
                source_revision: source_revision(1),
                tree_revision: tree_revision(tree),
            },
            source: Some(source),
            identity,
            equivalence,
        })
    }

    fn independent(
        provider_id: &str,
        publication: u64,
        symbol: &str,
        equivalence: Vec<EquivalenceEvidence>,
    ) -> Contribution {
        contribution(ContributionInput {
            provider: provider_id,
            publication,
            symbol,
            applicability: SourceApplicability::Independent,
            source: None,
            identity: None,
            equivalence,
        })
    }

    fn publication(
        provider_id: &str,
        revision: u64,
        contributions: Vec<Contribution>,
    ) -> ProviderPublication {
        ProviderPublication::new(
            provider(provider_id),
            provider_revision(revision),
            contributions,
            PublicationLimits::default(),
        )
        .expect("publication")
    }

    fn publications(values: Vec<ProviderPublication>) -> Arc<PublicationSet> {
        let mut set = PublicationSet::empty(PublicationLimits::default());
        for publication in values {
            set = set.replaced(publication).expect("publication set");
        }
        Arc::new(set)
    }

    fn normalize(
        publications: &Arc<PublicationSet>,
        tree: u64,
        previous: Option<&NormalizedGraph>,
    ) -> NormalizedGraph {
        Normalizer::normalize(
            index_revision(tree),
            source_revision(1),
            tree_revision(tree),
            publications,
            previous,
        )
        .expect("normalized graph")
    }

    #[test]
    fn applicability_excludes_another_tree_and_keeps_independent_facts() {
        let set = publications(vec![publication(
            "syntax",
            1,
            vec![
                exact(
                    "syntax",
                    1,
                    "one",
                    1,
                    binding("src/lib.rs", 0),
                    None,
                    vec![],
                ),
                exact(
                    "syntax",
                    1,
                    "two",
                    2,
                    binding("src/lib.rs", 8),
                    None,
                    vec![],
                ),
                independent("syntax", 1, "guide", vec![]),
            ],
        )]);
        let graph = normalize(&set, 1, None);
        assert_eq!(graph.records().len(), 2);
        assert!(graph.record_for(&reference("syntax", "one")).is_some());
        assert!(graph.record_for(&reference("syntax", "two")).is_none());
        assert!(graph.record_for(&reference("syntax", "guide")).is_some());
    }

    #[test]
    fn shared_declaration_binding_joins_provider_facts() {
        let declaration = binding("src/lib.rs", 0);
        let identity = "rift://symbol/rust/src/lib.rs/Beacon";
        let set = publications(vec![
            publication(
                "syntax",
                1,
                vec![exact(
                    "syntax",
                    1,
                    "Beacon",
                    1,
                    declaration.clone(),
                    Some(identity),
                    vec![],
                )],
            ),
            publication(
                "native",
                1,
                vec![exact("native", 1, "Beacon", 1, declaration, None, vec![])],
            ),
        ]);
        let graph = normalize(&set, 1, None);
        assert_eq!(graph.records().len(), 1);
        let record = &graph.records()[0];
        assert_eq!(record.identity().map(SymbolId::as_str), Some(identity));
        assert_eq!(record.contributions().len(), 2);
    }

    #[test]
    fn explicit_equivalence_attaches_revision_independent_facts() {
        let target = reference("syntax", "Beacon");
        let set = publications(vec![
            publication(
                "syntax",
                1,
                vec![exact(
                    "syntax",
                    1,
                    "Beacon",
                    1,
                    binding("src/lib.rs", 0),
                    Some("rift://symbol/rust/src/lib.rs/Beacon"),
                    vec![],
                )],
            ),
            publication(
                "docs",
                1,
                vec![independent(
                    "docs",
                    1,
                    "Beacon",
                    vec![EquivalenceEvidence::Explicit(target)],
                )],
            ),
        ]);
        let graph = normalize(&set, 1, None);
        assert_eq!(graph.records().len(), 1);
        assert_eq!(graph.records()[0].contributions().len(), 2);
    }

    #[test]
    fn candidate_evidence_never_establishes_identity() {
        let target = reference("syntax", "Beacon");
        let set = publications(vec![
            publication(
                "syntax",
                1,
                vec![independent("syntax", 1, "Beacon", vec![])],
            ),
            publication(
                "docs",
                1,
                vec![independent(
                    "docs",
                    1,
                    "Beacon",
                    vec![EquivalenceEvidence::Candidate(target)],
                )],
            ),
        ]);
        let graph = normalize(&set, 1, None);
        assert_eq!(graph.records().len(), 2);
        assert!(
            graph
                .records()
                .iter()
                .all(|record| { record.resolution() == SymbolResolution::Unresolved })
        );
        assert_eq!(graph.candidates().len(), 1);
        assert_eq!(graph.candidates()[0].state(), AssociationState::Candidate);
    }

    #[test]
    fn conflicting_anchors_remain_separate_records() {
        let syntax = reference("syntax", "Beacon");
        let set = publications(vec![
            publication(
                "syntax",
                1,
                vec![exact(
                    "syntax",
                    1,
                    "Beacon",
                    1,
                    binding("src/lib.rs", 0),
                    Some("rift://symbol/rust/src/lib.rs/Beacon"),
                    vec![],
                )],
            ),
            publication(
                "native",
                1,
                vec![exact(
                    "native",
                    1,
                    "Other",
                    1,
                    binding("src/other.rs", 0),
                    Some("rift://symbol/rust/src/other.rs/Other"),
                    vec![EquivalenceEvidence::Explicit(syntax)],
                )],
            ),
        ]);
        let graph = normalize(&set, 1, None);
        assert_eq!(graph.records().len(), 2);
        assert!(
            graph
                .records()
                .iter()
                .all(|record| { record.resolution() == SymbolResolution::Established })
        );
        assert_eq!(graph.candidates().len(), 1);
        assert_eq!(graph.candidates()[0].state(), AssociationState::Conflicting);
    }

    #[test]
    fn provider_local_continuity_retains_identity_across_publications() {
        let first_set = publications(vec![publication(
            "syntax",
            1,
            vec![exact(
                "syntax",
                1,
                "Beacon",
                1,
                binding("src/lib.rs", 0),
                Some("rift://symbol/rust/src/lib.rs/Beacon"),
                vec![],
            )],
        )]);
        let first = normalize(&first_set, 1, None);
        let second_set = publications(vec![publication(
            "syntax",
            2,
            vec![independent("syntax", 2, "Beacon", vec![])],
        )]);
        let second = normalize(&second_set, 2, Some(&first));
        assert_eq!(
            second
                .record_for(&reference("syntax", "Beacon"))
                .and_then(rift_core::SymbolRecord::identity)
                .map(SymbolId::as_str),
            Some("rift://symbol/rust/src/lib.rs/Beacon")
        );
    }
}
