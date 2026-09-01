use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rift_core::{
    Contribution, ContributionError, ContributionKey, ContributionReference, DeclarationBinding,
    EquivalenceEvidence, IndexRevision, ReferenceRole, RelationshipKind, SourceApplicability,
    SourceRevision, SourceUnitId, SymbolId, SymbolRecord, SymbolResolution, TreeRevision,
    symbol_identity,
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

/// Target retained after normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedTarget {
    /// Target record has established workspace identity.
    Symbol(SymbolId),
    /// Target remains provider-local.
    Contribution(ContributionReference),
}

/// One Relationship in a captured normalized graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedRelationship {
    index_revision: IndexRevision,
    source: ContributionKey,
    kind: RelationshipKind,
    target: NormalizedTarget,
}

impl NormalizedRelationship {
    /// Returns captured index revision.
    #[must_use]
    pub const fn index_revision(&self) -> IndexRevision {
        self.index_revision
    }

    /// Returns Contribution supplying Relationship.
    #[must_use]
    pub const fn source(&self) -> &ContributionKey {
        &self.source
    }

    /// Returns portable Relationship kind.
    #[must_use]
    pub const fn kind(&self) -> RelationshipKind {
        self.kind
    }

    /// Returns normalized target.
    #[must_use]
    pub const fn target(&self) -> &NormalizedTarget {
        &self.target
    }
}

/// One Reference in a captured normalized graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedReference {
    index_revision: IndexRevision,
    source: ContributionKey,
    binding: DeclarationBinding,
    role: ReferenceRole,
    targets: Vec<NormalizedTarget>,
}

impl NormalizedReference {
    /// Returns captured index revision.
    #[must_use]
    pub const fn index_revision(&self) -> IndexRevision {
        self.index_revision
    }

    /// Returns Contribution supplying Reference.
    #[must_use]
    pub const fn source(&self) -> &ContributionKey {
        &self.source
    }

    /// Returns source occurrence.
    #[must_use]
    pub const fn binding(&self) -> &DeclarationBinding {
        &self.binding
    }

    /// Returns portable role.
    #[must_use]
    pub const fn role(&self) -> ReferenceRole {
        self.role
    }

    /// Returns normalized targets.
    #[must_use]
    pub fn targets(&self) -> &[NormalizedTarget] {
        &self.targets
    }
}

/// Contributions available at one source position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    index_revision: IndexRevision,
    unit: SourceUnitId,
    position: u64,
    contributions: Vec<ContributionKey>,
}

impl Scope {
    /// Returns captured index revision.
    #[must_use]
    pub const fn index_revision(&self) -> IndexRevision {
        self.index_revision
    }

    /// Returns source unit.
    #[must_use]
    pub const fn unit(&self) -> &SourceUnitId {
        &self.unit
    }

    /// Returns UTF-8 byte position.
    #[must_use]
    pub const fn position(&self) -> u64 {
        self.position
    }

    /// Returns available Contributions.
    #[must_use]
    pub fn contributions(&self) -> &[ContributionKey] {
        &self.contributions
    }
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
    contribution_positions: BTreeMap<ContributionKey, usize>,
    candidates: Vec<AssociationCandidate>,
    references: Vec<NormalizedReference>,
    relationships: Vec<NormalizedRelationship>,
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

    /// Returns captured Contribution by immutable key, through the position
    /// index assembly built: the workspace map resolves a contribution per
    /// record and per reference, and a scan per lookup made one map build
    /// cost O(records x contributions).
    #[must_use]
    pub fn contribution(&self, key: &ContributionKey) -> Option<&Contribution> {
        let position = *self.contribution_positions.get(key)?;
        self.publications
            .provider(key.reference().provider())
            .and_then(|publication| publication.contributions().get(position))
    }

    /// Returns normalized References.
    #[must_use]
    pub fn references(&self) -> &[NormalizedReference] {
        &self.references
    }

    /// Returns normalized Relationships.
    #[must_use]
    pub fn relationships(&self) -> &[NormalizedRelationship] {
        &self.relationships
    }

    /// Builds availability at one source position.
    #[must_use]
    pub fn scope_at(&self, unit: SourceUnitId, position: u64) -> Scope {
        let mut contributions = self
            .publications
            .publications()
            .flat_map(crate::ProviderPublication::contributions)
            .filter(|contribution| {
                if !contribution
                    .applicability()
                    .applies_to(self.source_revision, self.tree_revision)
                {
                    return false;
                }
                contribution.source().map_or(
                    matches!(
                        contribution.applicability(),
                        SourceApplicability::Independent
                    ),
                    |binding| {
                        binding.unit() == &unit
                            && binding.range().start() <= position
                            && position < binding.range().end()
                    },
                )
            })
            .map(|contribution| contribution.key().clone())
            .collect::<Vec<_>>();
        contributions.sort();
        Scope {
            index_revision: self.index_revision,
            unit,
            position,
            contributions,
        }
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
        && let Some(facts) = contribution.facts()
    {
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

fn build_records(
    index_revision: IndexRevision,
    contributions: &[&Contribution],
    anchors: &[BTreeSet<SymbolId>],
    groups: &mut UnionFind,
    conflicting: &BTreeSet<usize>,
) -> Result<(Vec<SymbolRecord>, BTreeMap<ContributionReference, usize>), ContributionError> {
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
    Ok((records, records_by_contribution))
}

fn normalize_edges(
    index_revision: IndexRevision,
    contributions: &[&Contribution],
    records: &[SymbolRecord],
    records_by_contribution: &BTreeMap<ContributionReference, usize>,
) -> (Vec<NormalizedReference>, Vec<NormalizedRelationship>) {
    let normalize_target = |target: &ContributionReference| {
        records_by_contribution
            .get(target)
            .and_then(|index| records.get(*index))
            .and_then(SymbolRecord::identity)
            .cloned()
            .map_or_else(
                || NormalizedTarget::Contribution(target.clone()),
                NormalizedTarget::Symbol,
            )
    };
    let mut references = Vec::new();
    let mut relationships = Vec::new();
    for contribution in contributions {
        references.extend(
            contribution
                .references()
                .iter()
                .map(|reference| NormalizedReference {
                    index_revision,
                    source: contribution.key().clone(),
                    binding: reference.source().clone(),
                    role: reference.role(),
                    targets: reference.targets().iter().map(&normalize_target).collect(),
                }),
        );
        relationships.extend(contribution.relationships().iter().map(|relationship| {
            NormalizedRelationship {
                index_revision,
                source: contribution.key().clone(),
                kind: relationship.kind(),
                target: normalize_target(relationship.target()),
            }
        }));
    }
    (references, relationships)
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
    let (records, records_by_contribution) = build_records(
        index_revision,
        contributions,
        anchors,
        &mut groups,
        conflicting,
    )?;
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
    let (references, relationships) = normalize_edges(
        index_revision,
        contributions,
        &records,
        &records_by_contribution,
    );

    let contribution_positions = contribution_positions(&publications);
    Ok(NormalizedGraph {
        index_revision,
        source_revision,
        tree_revision,
        publications,
        records,
        records_by_contribution,
        contribution_positions,
        candidates,
        references,
        relationships,
    })
}

/// Positions every publication's contributions by key, once per assembled
/// graph. Each key already names its provider, so the position within that
/// provider's contribution list is the only fact a lookup still needs.
fn contribution_positions(publications: &PublicationSet) -> BTreeMap<ContributionKey, usize> {
    let mut positions = BTreeMap::new();
    for publication in publications.publications() {
        for (position, contribution) in publication.contributions().iter().enumerate() {
            positions.insert(contribution.key().clone(), position);
        }
    }
    positions
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
        ContributionRelationship, DeclarationBinding, EquivalenceEvidence, ExactKind,
        IndexRevision, Language, PortableSymbolFacts, ProviderId, ProviderRevision,
        ProviderSymbolId, ReferenceRole, RelationshipKind, SemanticReference, SourceApplicability,
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

    /// Every published contribution resolves through the position index by
    /// its own exact key, and a key the set never published answers `None`
    /// instead of a neighbor at the same position.
    #[test]
    fn contribution_lookup_answers_each_published_key_and_refuses_foreign_ones() {
        let set = publications(vec![
            publication(
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
                ],
            ),
            publication(
                "native",
                1,
                vec![exact(
                    "native",
                    1,
                    "one",
                    1,
                    binding("src/lib.rs", 0),
                    None,
                    vec![],
                )],
            ),
        ]);
        let graph = normalize(&set, 1, None);
        for provider_publication in set.publications() {
            for contribution in provider_publication.contributions() {
                let answered = graph
                    .contribution(contribution.key())
                    .expect("every published key resolves");
                assert_eq!(
                    answered.key(),
                    contribution.key(),
                    "the lookup answers the exact key it was asked for"
                );
            }
        }
        let unpublished = exact(
            "syntax",
            2,
            "one",
            1,
            binding("src/lib.rs", 0),
            None,
            vec![],
        );
        assert!(
            graph.contribution(unpublished.key()).is_none(),
            "a key the set never published resolves to nothing"
        );
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

    #[test]
    fn references_relationships_and_scope_keep_resolution_state() {
        let target = reference("syntax", "Beacon");
        let syntax = exact(
            "syntax",
            1,
            "Beacon",
            1,
            binding("src/lib.rs", 0),
            Some("rift://symbol/rust/src/lib.rs/Beacon"),
            vec![],
        );
        let semantic_base = exact(
            "native",
            1,
            "caller",
            1,
            binding("src/lib.rs", 16),
            None,
            vec![],
        );
        let semantic = Contribution::builder(
            semantic_base.key().clone(),
            semantic_base.applicability(),
            semantic_base.facts().expect("portable facts").clone(),
            semantic_base.origin().clone(),
        )
        .source(semantic_base.source().expect("source").clone())
        .references(vec![
            SemanticReference::new(
                binding("src/lib.rs", 24),
                ReferenceRole::Read,
                vec![target.clone()],
            )
            .expect("reference"),
        ])
        .relationships(vec![
            ContributionRelationship::new(RelationshipKind::Reference, target),
            ContributionRelationship::new(
                RelationshipKind::Implementation,
                reference("missing", "Trait"),
            ),
        ])
        .build()
        .expect("semantic contribution");
        let set = publications(vec![
            publication("syntax", 1, vec![syntax]),
            publication("native", 1, vec![semantic]),
            publication("docs", 1, vec![independent("docs", 1, "guide", vec![])]),
        ]);
        let graph = normalize(&set, 1, None);

        assert!(matches!(
            graph.references()[0].targets(),
            [super::NormalizedTarget::Symbol(identity)]
                if identity.as_str() == "rift://symbol/rust/src/lib.rs/Beacon"
        ));
        assert!(matches!(
            graph.relationships()[0].target(),
            super::NormalizedTarget::Symbol(identity)
                if identity.as_str() == "rift://symbol/rust/src/lib.rs/Beacon"
        ));
        assert!(matches!(
            graph.relationships()[1].target(),
            super::NormalizedTarget::Contribution(target)
                if target.provider().as_str() == "missing"
        ));

        let inside = graph.scope_at(source_unit("src/lib.rs"), 2);
        assert_eq!(inside.index_revision(), graph.index_revision());
        assert_eq!(inside.position(), 2);
        assert_eq!(inside.unit(), &source_unit("src/lib.rs"));
        assert_eq!(inside.contributions().len(), 2);
        let outside = graph.scope_at(source_unit("src/lib.rs"), 100);
        assert_eq!(outside.contributions().len(), 1);
        assert_eq!(
            outside.contributions()[0].reference().provider().as_str(),
            "docs"
        );
    }
}
