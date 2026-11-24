use std::collections::{BTreeMap, BTreeSet};

use rift_core::{
    Contribution, ContributionKey, ContributionOrigin, Extensions, IndexRevision,
    PortableSymbolFacts, ProviderId, SymbolId, SymbolRecord, SymbolResolution,
};
use rift_protocol::read::{SourceLocationKind, Symbol, SymbolId as WireSymbolId, SymbolOrigin};

use crate::NormalizedGraph;

/// Presentation field whose retained Contributions disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PresentationField {
    /// Language differs.
    Language,
    /// Short name differs.
    Name,
    /// Provider-qualified name differs.
    QualifiedName,
    /// Exact kind differs.
    Kind,
    /// Containing symbol differs.
    Container,
    /// Visibility differs.
    Visibility,
    /// Document-local classification differs.
    DocumentLocal,
    /// Origin differs.
    Origin,
}

/// One disagreement retained beside selected presentation facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentationDisagreement {
    contribution: ContributionKey,
    field: PresentationField,
}

impl PresentationDisagreement {
    /// Returns Contribution carrying alternate value.
    #[must_use]
    pub const fn contribution(&self) -> &ContributionKey {
        &self.contribution
    }

    /// Returns field that differs.
    #[must_use]
    pub const fn field(&self) -> PresentationField {
        self.field
    }
}

/// Readable symbol assembled from one normalized record.
#[derive(Debug, Clone)]
pub struct AssembledSymbol {
    index_revision: IndexRevision,
    identity: Option<SymbolId>,
    resolution: SymbolResolution,
    contributions: Vec<ContributionKey>,
    facts: Option<PortableSymbolFacts>,
    origin: ContributionOrigin,
    container: Option<SymbolId>,
    references: Vec<crate::NormalizedReference>,
    edges: Vec<crate::NormalizedEdge>,
    namespaced: Vec<(ProviderId, Extensions)>,
    namespaced_material_bytes: usize,
    disagreements: Vec<PresentationDisagreement>,
}

impl PartialEq for AssembledSymbol {
    fn eq(&self, other: &Self) -> bool {
        self.index_revision == other.index_revision
            && self.identity == other.identity
            && self.resolution == other.resolution
            && self.contributions == other.contributions
            && self.facts == other.facts
            && self.origin == other.origin
            && self.container == other.container
            && self.references == other.references
            && self.edges == other.edges
            && self.namespaced == other.namespaced
            && self.disagreements == other.disagreements
    }
}

impl AssembledSymbol {
    /// Returns captured index revision.
    #[must_use]
    pub const fn index_revision(&self) -> IndexRevision {
        self.index_revision
    }

    /// Returns established identity when available.
    #[must_use]
    pub const fn identity(&self) -> Option<&SymbolId> {
        self.identity.as_ref()
    }

    /// Returns normalized resolution state.
    #[must_use]
    pub const fn resolution(&self) -> SymbolResolution {
        self.resolution
    }

    /// Returns associated Contribution keys in presentation order.
    #[must_use]
    pub fn contributions(&self) -> &[ContributionKey] {
        &self.contributions
    }

    /// Returns selected and combined portable facts when supplied.
    #[must_use]
    pub const fn facts(&self) -> Option<&PortableSymbolFacts> {
        self.facts.as_ref()
    }

    /// Returns selected origin.
    #[must_use]
    pub const fn origin(&self) -> &ContributionOrigin {
        &self.origin
    }

    /// Returns normalized containing symbol identity.
    #[must_use]
    pub const fn container(&self) -> Option<&SymbolId> {
        self.container.as_ref()
    }

    /// Returns normalized References.
    #[must_use]
    pub fn references(&self) -> &[crate::NormalizedReference] {
        &self.references
    }

    /// Returns normalized Edges.
    #[must_use]
    pub fn edges(&self) -> &[crate::NormalizedEdge] {
        &self.edges
    }

    /// Returns every provider-specific fact collection.
    #[must_use]
    pub fn namespaced(&self) -> &[(ProviderId, Extensions)] {
        &self.namespaced
    }

    /// Returns retained presentation disagreements.
    #[must_use]
    pub fn disagreements(&self) -> &[PresentationDisagreement] {
        &self.disagreements
    }

    /// Converts normalized provider facts into one read symbol.
    #[must_use]
    pub fn to_protocol_symbol(&self, facts: &PortableSymbolFacts) -> Symbol {
        let mut extension_values = BTreeMap::new();
        for (_, extensions) in &self.namespaced {
            for (key, value) in &extensions.0 {
                extension_values
                    .entry(key.clone())
                    .or_insert_with(|| value.clone());
            }
        }
        let mut symbol = Self::wire_facts(facts, &self.origin);
        symbol.id = self
            .identity
            .as_ref()
            .map(|identity| WireSymbolId(identity.as_str().to_owned()));
        symbol.container = self
            .container
            .as_ref()
            .map(|container| WireSymbolId(container.as_str().to_owned()));
        symbol.extensions = Extensions(extension_values);
        symbol
    }

    /// Converts retained facts without assigning a logical identity or container.
    /// Portable Contribution admission remains separate from this factual projection.
    #[must_use]
    pub fn wire_facts(facts: &PortableSymbolFacts, origin: &ContributionOrigin) -> Symbol {
        Symbol {
            id: None,
            language: facts.language().clone(),
            name: facts.name().to_owned(),
            kind: facts.kind().clone(),
            kinds: facts.symbol_kinds().to_vec(),
            origin: wire_symbol_origin(origin),
            container: None,
            modifiers: facts.modifier_words().to_vec(),
            visibility: facts.visibility_spelling().map(str::to_owned),
            types: facts.type_bindings().to_vec(),
            signatures: facts.signatures_slice().to_vec(),
            documentation: facts.documentation_blocks().to_vec(),
            extensions: Extensions::default(),
            document_local: facts.is_document_local(),
        }
    }

    /// Converts a captured Contribution origin using the same read representation as
    /// logical objects and their physical declarations.
    #[must_use]
    pub fn wire_origin(origin: &ContributionOrigin) -> SymbolOrigin {
        wire_symbol_origin(origin)
    }
}

fn wire_symbol_origin(origin: &ContributionOrigin) -> SymbolOrigin {
    let location = origin.location();
    SymbolOrigin {
        location: location.map(|location| match location {
            rift_core::SourceLocation::Project { .. } => SourceLocationKind::Project,
            rift_core::SourceLocation::Dependency { .. } => SourceLocationKind::Dependency,
            rift_core::SourceLocation::Stdlib { .. } => SourceLocationKind::Stdlib,
            rift_core::SourceLocation::External {} => SourceLocationKind::External,
        }),
        package: location.and_then(|location| match location {
            rift_core::SourceLocation::Project { package } => package.clone(),
            rift_core::SourceLocation::Dependency { package } => Some(package.clone()),
            rift_core::SourceLocation::Stdlib { .. } | rift_core::SourceLocation::External {} => {
                None
            }
        }),
        runtime: location.and_then(|location| match location {
            rift_core::SourceLocation::Stdlib { runtime } => runtime.clone(),
            _ => None,
        }),
        source_kind: origin.source_kind(),
    }
}

/// Positions of a graph's references and edges by the Contribution that supplies them.
///
/// [`SymbolAssembler::assemble`] reads every reference and every edge of the graph for one
/// record. A caller that assembles every record of a graph builds this once, and
/// [`SymbolAssembler::assemble_from`] then reads the references and edges of the record's
/// own Contributions.
#[derive(Debug)]
pub struct AssemblySources<'graph> {
    references: BTreeMap<&'graph ContributionKey, Vec<usize>>,
    edges: BTreeMap<&'graph ContributionKey, Vec<usize>>,
}

impl<'graph> AssemblySources<'graph> {
    /// Reads every reference and edge of `graph` once.
    #[must_use]
    pub fn new(graph: &'graph NormalizedGraph) -> Self {
        let mut references: BTreeMap<_, Vec<usize>> = BTreeMap::new();
        for (position, reference) in graph.references().iter().enumerate() {
            references
                .entry(reference.source())
                .or_default()
                .push(position);
        }
        let mut edges: BTreeMap<_, Vec<usize>> = BTreeMap::new();
        for (position, edge) in graph.edges().iter().enumerate() {
            edges.entry(edge.source()).or_default().push(position);
        }
        Self { references, edges }
    }

    /// Positions the Contributions of `record` supply, in graph order.
    fn positions(
        by_source: &BTreeMap<&'graph ContributionKey, Vec<usize>>,
        record: &SymbolRecord,
    ) -> Vec<usize> {
        let mut positions: Vec<usize> = record
            .contributions()
            .iter()
            .filter_map(|key| by_source.get(key))
            .flatten()
            .copied()
            .collect();
        positions.sort_unstable();
        positions.dedup();
        positions
    }
}

/// Deterministic readable Symbol assembly.
#[derive(Debug)]
pub struct SymbolAssembler;

impl SymbolAssembler {
    /// Assembles one normalized record.
    ///
    /// Providers named earlier in `precedence` select scalar presentation
    /// fields. Unnamed providers follow in provider identity order, and within one
    /// provider [`NormalizedGraph::definition_ranks`]: no declaration fills a scalar
    /// field one of a lower rank leaves out. Lists combine in same order without
    /// duplicates. Missing captured Contributions return `None`.
    #[must_use]
    pub fn assemble(
        graph: &NormalizedGraph,
        record: &SymbolRecord,
        precedence: &[ProviderId],
    ) -> Option<AssembledSymbol> {
        Self::assemble_with(graph, record, precedence, None)
    }

    /// Assembles one normalized record of the graph `sources` was built from.
    ///
    /// Answers what [`Self::assemble`] answers. It reads the references and edges of the
    /// record's Contributions from `sources`, so assembling every record costs the graph's
    /// references and edges once.
    #[must_use]
    pub fn assemble_from(
        graph: &NormalizedGraph,
        sources: &AssemblySources<'_>,
        record: &SymbolRecord,
        precedence: &[ProviderId],
    ) -> Option<AssembledSymbol> {
        Self::assemble_with(graph, record, precedence, Some(sources))
    }

    fn assemble_with(
        graph: &NormalizedGraph,
        record: &SymbolRecord,
        precedence: &[ProviderId],
        sources: Option<&AssemblySources<'_>>,
    ) -> Option<AssembledSymbol> {
        if record.index_revision() != graph.index_revision() {
            return None;
        }
        let captured = record
            .contributions()
            .iter()
            .map(|key| graph.contribution(key))
            .collect::<Option<Vec<_>>>()?;
        let mut ranked = match captured.as_slice() {
            [only] => vec![(0, *only)],
            several => graph
                .definition_ranks(several)
                .into_iter()
                .zip(several.iter().copied())
                .collect(),
        };
        ranked.sort_by_cached_key(|(rank, contribution)| {
            let key = contribution.key();
            (provider_rank(key, precedence), *rank, key.clone())
        });
        let contributions = ranked
            .iter()
            .map(|(_, contribution)| (contribution.key().clone(), *contribution))
            .collect::<Vec<_>>();
        let primary = contributions.first()?.1;
        let selecting = selecting_facts(&ranked);
        let primary_facts = selecting.first().copied();
        let selected_visibility = selecting
            .iter()
            .find_map(|facts| facts.visibility_spelling());
        let selected_container = selecting
            .iter()
            .find_map(|facts| facts.container_reference());
        let facts = primary_facts.map(|facts| {
            combined_facts(
                &contributions,
                facts,
                selected_visibility,
                selected_container,
            )
        });
        let container =
            selected_container.and_then(|reference| graph.identity_for(reference).cloned());
        let disagreements = primary_facts.map_or_else(Vec::new, |facts| {
            presentation_disagreements(
                &contributions,
                primary,
                facts,
                selected_visibility,
                selected_container,
            )
        });
        let namespaced_material_bytes = contributions.iter().fold(0usize, |bytes, (_, value)| {
            bytes.saturating_add(value.namespaced_material_bytes())
        });
        let namespaced = contributions
            .iter()
            .map(|(key, contribution)| {
                (
                    key.reference().provider().clone(),
                    contribution.namespaced().clone(),
                )
            })
            .collect();
        let (references, edges) = relationships(graph, record, sources, &contributions);
        Some(AssembledSymbol {
            index_revision: record.index_revision(),
            identity: record.identity().cloned(),
            resolution: record.resolution(),
            contributions: contributions.into_iter().map(|(key, _)| key).collect(),
            facts,
            origin: primary.origin().clone(),
            container,
            references,
            edges,
            namespaced,
            namespaced_material_bytes,
            disagreements,
        })
    }

    /// Whether [`Self::assemble`] returns a symbol with portable facts for `record`.
    ///
    /// Decides the answer from the record's captured Contributions alone: it builds no
    /// references, edges, or presentation, so its cost is the record's Contribution count
    /// instead of the graph's reference and edge count.
    #[must_use]
    pub fn assembles_facts(graph: &NormalizedGraph, record: &SymbolRecord) -> bool {
        if record.index_revision() != graph.index_revision() {
            return false;
        }
        let mut facts = false;
        for key in record.contributions() {
            let Some(contribution) = graph.contribution(key) else {
                return false;
            };
            facts |= contribution.facts().is_some();
        }
        facts
    }
}

fn relationships(
    graph: &NormalizedGraph,
    record: &SymbolRecord,
    sources: Option<&AssemblySources<'_>>,
    contributions: &[(ContributionKey, &Contribution)],
) -> (Vec<crate::NormalizedReference>, Vec<crate::NormalizedEdge>) {
    match sources {
        Some(sources) => (
            AssemblySources::positions(&sources.references, record)
                .into_iter()
                .filter_map(|position| graph.references().get(position))
                .cloned()
                .collect(),
            AssemblySources::positions(&sources.edges, record)
                .into_iter()
                .filter_map(|position| graph.edges().get(position))
                .cloned()
                .collect(),
        ),
        None if contributions.iter().all(|(_, contribution)| {
            contribution.references().is_empty() && contribution.edges().is_empty()
        }) =>
        {
            (Vec::new(), Vec::new())
        }
        None => (
            graph
                .references()
                .iter()
                .filter(|reference| record.contributions().contains(reference.source()))
                .cloned()
                .collect(),
            graph
                .edges()
                .iter()
                .filter(|edge| record.contributions().contains(edge.source()))
                .cloned()
                .collect(),
        ),
    }
}

/// The facts scalar fields are selected from, in presentation order: none of a higher rank
/// than the first, so an implementation fills no field a definition leaves out.
fn selecting_facts<'graph>(
    ranked: &[(u8, &'graph Contribution)],
) -> Vec<&'graph PortableSymbolFacts> {
    let mut supplied = ranked
        .iter()
        .filter_map(|(rank, contribution)| Some((*rank, contribution.facts()?)))
        .peekable();
    let Some((first, _)) = supplied.peek().copied() else {
        return Vec::new();
    };
    supplied
        .filter(|(rank, _)| *rank <= first)
        .map(|(_, facts)| facts)
        .collect()
}

fn provider_rank(key: &ContributionKey, precedence: &[ProviderId]) -> (usize, ProviderId) {
    let provider = key.reference().provider();
    (
        precedence
            .iter()
            .position(|candidate| candidate == provider)
            .unwrap_or(usize::MAX),
        provider.clone(),
    )
}

fn combined_facts(
    contributions: &[(ContributionKey, &Contribution)],
    primary: &PortableSymbolFacts,
    visibility: Option<&str>,
    container: Option<&rift_core::ContributionReference>,
) -> PortableSymbolFacts {
    let mut facts = PortableSymbolFacts::new(
        primary.language().clone(),
        primary.name(),
        primary.qualified_name(),
        primary.kind().clone(),
    )
    .kinds(unique_values(contributions, |facts| facts.symbol_kinds()))
    .modifiers(unique_values(contributions, |facts| facts.modifier_words()))
    .types(unique_values(contributions, |facts| facts.type_bindings()))
    .signatures(unique_values(contributions, |facts| {
        facts.signatures_slice()
    }))
    .documentation(unique_values(contributions, |facts| {
        facts.documentation_blocks()
    }))
    .document_local(primary.is_document_local());
    if let Some(visibility) = visibility {
        facts = facts.visibility(visibility);
    }
    if let Some(container) = container {
        facts = facts.container(container.clone());
    }
    facts
}

fn unique_values<T: Clone + PartialEq>(
    contributions: &[(ContributionKey, &Contribution)],
    values: impl Fn(&PortableSymbolFacts) -> &[T],
) -> Vec<T> {
    let mut combined = Vec::new();
    for (_, contribution) in contributions {
        let Some(facts) = contribution.facts() else {
            continue;
        };
        for value in values(facts) {
            if !combined.contains(value) {
                combined.push(value.clone());
            }
        }
    }
    combined
}

fn presentation_disagreements(
    contributions: &[(ContributionKey, &Contribution)],
    primary: &Contribution,
    selected: &PortableSymbolFacts,
    visibility: Option<&str>,
    container: Option<&rift_core::ContributionReference>,
) -> Vec<PresentationDisagreement> {
    let mut disagreements = BTreeSet::new();
    for (key, contribution) in contributions {
        // One provider can describe distinct declarations of the same symbol.
        // Their declaration kinds and visibility need not agree.
        let same_provider = key.reference().provider() == primary.key().reference().provider();
        let different_declarations = contribution
            .source()
            .zip(primary.source())
            .is_some_and(|(left, right)| left != right);
        if same_provider && different_declarations {
            continue;
        }
        if let Some(facts) = contribution.facts() {
            retain_difference(
                &mut disagreements,
                key,
                PresentationField::Language,
                facts.language() != selected.language(),
            );
            retain_difference(
                &mut disagreements,
                key,
                PresentationField::Name,
                facts.name() != selected.name(),
            );
            retain_difference(
                &mut disagreements,
                key,
                PresentationField::QualifiedName,
                facts.qualified_name() != selected.qualified_name(),
            );
            retain_difference(
                &mut disagreements,
                key,
                PresentationField::Kind,
                facts.kind() != selected.kind(),
            );
            retain_difference(
                &mut disagreements,
                key,
                PresentationField::Container,
                facts.container_reference() != container,
            );
            retain_difference(
                &mut disagreements,
                key,
                PresentationField::Visibility,
                facts.visibility_spelling() != visibility,
            );
            retain_difference(
                &mut disagreements,
                key,
                PresentationField::DocumentLocal,
                facts.is_document_local() != selected.is_document_local(),
            );
        }
        retain_difference(
            &mut disagreements,
            key,
            PresentationField::Origin,
            contribution.origin() != primary.origin(),
        );
    }
    disagreements
        .into_iter()
        .map(|(contribution, field)| PresentationDisagreement {
            contribution,
            field,
        })
        .collect()
}

fn retain_difference(
    disagreements: &mut BTreeSet<(ContributionKey, PresentationField)>,
    key: &ContributionKey,
    field: PresentationField,
    differs: bool,
) {
    if differs {
        disagreements.insert((key.clone(), field));
    }
}

impl get_size2::GetSize for PresentationDisagreement {
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
        let (field, next) =
            get_size2::GetSize::get_heap_size_with_tracker(&self.contribution, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        (bytes, tracker)
    }
}

impl get_size2::GetSize for AssembledSymbol {
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
        let (field, next) = get_size2::GetSize::get_heap_size_with_tracker(&self.identity, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) =
            get_size2::GetSize::get_heap_size_with_tracker(&self.contributions, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = get_size2::GetSize::get_heap_size_with_tracker(&self.facts, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = get_size2::GetSize::get_heap_size_with_tracker(&self.origin, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) =
            get_size2::GetSize::get_heap_size_with_tracker(&self.container, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) =
            get_size2::GetSize::get_heap_size_with_tracker(&self.references, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = get_size2::GetSize::get_heap_size_with_tracker(&self.edges, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        bytes = bytes
            .saturating_add(self.namespaced_material_bytes)
            .saturating_add(
                self.namespaced
                    .capacity()
                    .saturating_mul(std::mem::size_of::<(ProviderId, Extensions)>()),
            );
        for (provider, _) in &self.namespaced {
            let (field, next) = get_size2::GetSize::get_heap_size_with_tracker(provider, tracker);
            bytes = bytes.saturating_add(field);
            tracker = next;
        }
        let (field, next) =
            get_size2::GetSize::get_heap_size_with_tracker(&self.disagreements, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        (bytes, tracker)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use rift_core::{
        Contribution, ContributionKey, ContributionOrigin, ContributionReference, ExactKind,
        IndexRevision, PortableSymbolFacts, ProviderId, ProviderRevision, ProviderSymbolId,
        SourceApplicability, SourceKind, SourceLocation, SourceRange, SourceRevision, SourceUnitId,
        SymbolId, SymbolKind, TreeRevision,
    };
    use rift_core::{
        ContributionEdge, DeclarationBinding, Documentation, DocumentationFormat, EdgeKind,
        Extensions, Language, ReferenceRole, SemanticReference,
    };

    use super::{AssemblySources, PresentationField, SymbolAssembler};
    use crate::{Normalizer, ProviderPublication, PublicationLimits, PublicationSet};

    fn provider(value: &str) -> ProviderId {
        ProviderId::new(value).expect("provider")
    }

    fn reference(provider_id: &str, symbol: &str) -> ContributionReference {
        ContributionReference::new(
            provider(provider_id),
            ProviderSymbolId::new(symbol).expect("provider symbol"),
        )
    }

    fn contribution(
        provider_id: &str,
        revision: u64,
        symbol: &str,
        name: &str,
        identity: Option<&str>,
        container: Option<ContributionReference>,
    ) -> Contribution {
        let provider = provider(provider_id);
        let mut facts = PortableSymbolFacts::new(
            Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            name,
            format!("crate::{name}"),
            ExactKind("function".to_owned()),
        )
        .kinds(vec![SymbolKind::Value])
        .documentation(vec![Documentation {
            format: DocumentationFormat::Markdown,
            text: format!("{provider_id} docs"),
        }]);
        if let Some(container) = container {
            facts = facts.container(container);
        }
        let mut builder = Contribution::builder(
            ContributionKey::new(
                provider,
                ProviderRevision::new(revision).expect("publication"),
                ProviderSymbolId::new(symbol).expect("provider symbol"),
            ),
            SourceApplicability::Exact {
                source_revision: SourceRevision::new(1).expect("source"),
                tree_revision: TreeRevision::new(1).expect("tree"),
            },
            facts,
            ContributionOrigin::new(
                Some(SourceLocation::Project { package: None }),
                SourceKind::Authored,
            )
            .expect("origin"),
        )
        .namespaced(Extensions(BTreeMap::default()));
        if let Some(identity) = identity {
            builder = builder
                .source(rift_core::DeclarationBinding::new(
                    SourceUnitId::parse("rift://source/rift.sources.project/src/lib.rs")
                        .expect("source unit"),
                    SourceRange::new(0, 8).expect("range"),
                ))
                .identity_anchor(SymbolId::new(identity).expect("identity"));
        }
        builder.build().expect("contribution")
    }

    fn graph() -> crate::NormalizedGraph {
        let syntax = contribution(
            "syntax",
            1,
            "syntax-beacon",
            "Beacon",
            Some("rift://symbol/local/rust/app/Beacon"),
            None,
        );
        let lsp = contribution(
            "lsp",
            1,
            "lsp-beacon",
            "beacon",
            None,
            Some(reference("syntax", "syntax-container")),
        );
        let lsp = Contribution::builder(
            lsp.key().clone(),
            lsp.applicability(),
            lsp.facts().expect("portable facts").clone(),
            lsp.origin().clone(),
        )
        .equivalence(vec![rift_core::EquivalenceEvidence::Explicit(reference(
            "syntax",
            "syntax-beacon",
        ))])
        .namespaced(lsp.namespaced().clone())
        .build()
        .expect("lsp contribution");
        let limits = PublicationLimits::default();
        let syntax = ProviderPublication::new(
            provider("syntax"),
            ProviderRevision::new(1).expect("revision"),
            vec![syntax],
            limits,
        )
        .expect("syntax publication");
        let lsp = ProviderPublication::new(
            provider("lsp"),
            ProviderRevision::new(1).expect("revision"),
            vec![lsp],
            limits,
        )
        .expect("lsp publication");
        let set = PublicationSet::empty(limits)
            .replaced(lsp)
            .and_then(|set| set.replaced(syntax))
            .expect("publications");
        Normalizer::normalize(
            IndexRevision::new(1).expect("index"),
            SourceRevision::new(1).expect("source"),
            TreeRevision::new(1).expect("tree"),
            &Arc::new(set),
            None,
        )
        .expect("graph")
    }

    #[test]
    fn source_less_raw_facts_keep_full_names_signatures_and_documentation() {
        let origin = ContributionOrigin::new(None, SourceKind::Synthetic).expect("origin");
        for characters in [3000, 4097] {
            let name = "𐐀".repeat(characters);
            let signature = serde_json::from_value(serde_json::json!({
                "display": format!("fn {name}()"), "language":"rust",
            }))
            .expect("signature");
            let facts = PortableSymbolFacts::new(
                Language {
                    name: "rust".to_owned(),
                    dialect: None,
                },
                &name,
                &name,
                ExactKind("function".to_owned()),
            )
            .signatures(vec![signature])
            .documentation(vec![Documentation {
                format: DocumentationFormat::Markdown,
                text: "Original source-less documentation.".to_owned(),
            }]);
            assert!(!rift_core::is_portable_name(&name));
            let refused = Contribution::builder(
                ContributionKey::new(
                    provider("syntax"),
                    ProviderRevision::new(1).expect("revision"),
                    ProviderSymbolId::new("raw").expect("provider symbol"),
                ),
                SourceApplicability::Independent,
                facts.clone(),
                origin.clone(),
            )
            .build();
            assert!(
                refused.is_err(),
                "raw facts cannot bypass portable admission"
            );
            let symbol = super::AssembledSymbol::wire_facts(&facts, &origin);
            assert_eq!(symbol.id, None);
            assert_eq!(symbol.container, None);
            assert_eq!(symbol.name, name);
            assert_eq!(symbol.name_is_public(), characters <= 4096);
            assert!(symbol.name_is_retained(name.len()));
            assert!(!symbol.name_is_retained(name.len() - 1));
            assert_eq!(symbol.signatures.len(), 1);
            assert_eq!(
                symbol.documentation[0].text,
                "Original source-less documentation."
            );
            assert_eq!(symbol.origin.source_kind, SourceKind::Synthetic);
            let wire = serde_json::to_value(&symbol).expect("retained symbol serializes");
            assert_eq!(
                serde_json::from_value::<rift_protocol::read::Symbol>(wire)
                    .expect("retained symbol roundtrip"),
                symbol
            );
        }
    }

    /// A graph whose Contributions supply references and edges, several per record.
    fn linked_graph() -> crate::NormalizedGraph {
        let limits = PublicationLimits::default();
        let linked = |symbol: &str, name: &str, targets: &[&str]| {
            let plain = contribution(
                "syntax",
                1,
                symbol,
                name,
                Some(&format!("rift://symbol/local/rust/app/{name}")),
                None,
            );
            let references = targets
                .iter()
                .enumerate()
                .map(|(position, target)| {
                    let start = u64::try_from(position).expect("position") * 10 + 20;
                    SemanticReference::new(
                        DeclarationBinding::new(
                            SourceUnitId::parse("rift://source/rift.sources.project/src/lib.rs")
                                .expect("source unit"),
                            SourceRange::new(start, start + 4).expect("range"),
                        ),
                        ReferenceRole::Call,
                        vec![reference("syntax", target)],
                    )
                    .expect("reference")
                })
                .collect();
            let edges = targets
                .iter()
                .map(|target| {
                    ContributionEdge::new(EdgeKind::Implementation, reference("syntax", target))
                })
                .collect();
            Contribution::builder(
                plain.key().clone(),
                plain.applicability(),
                plain.facts().expect("portable facts").clone(),
                plain.origin().clone(),
            )
            .source(plain.source().expect("source").clone())
            .identity_anchor(plain.identity_anchor().expect("identity").clone())
            .namespaced(plain.namespaced().clone())
            .references(references)
            .edges(edges)
            .build()
            .expect("linked contribution")
        };
        let syntax = ProviderPublication::new(
            provider("syntax"),
            ProviderRevision::new(1).expect("revision"),
            vec![
                linked("syntax-alpha", "Alpha", &["syntax-beta", "syntax-gamma"]),
                linked("syntax-beta", "Beta", &["syntax-gamma"]),
                linked("syntax-gamma", "Gamma", &[]),
                linked("syntax-delta", "Delta", &["syntax-alpha", "syntax-beta"]),
            ],
            limits,
        )
        .expect("syntax publication");
        let set = PublicationSet::empty(limits)
            .replaced(syntax)
            .expect("publications");
        Normalizer::normalize(
            IndexRevision::new(1).expect("index"),
            SourceRevision::new(1).expect("source"),
            TreeRevision::new(1).expect("tree"),
            &Arc::new(set),
            None,
        )
        .expect("graph")
    }

    #[test]
    fn records_assembled_from_sources_equal_records_assembled_one_at_a_time() {
        for graph in [graph(), linked_graph()] {
            let sources = AssemblySources::new(&graph);
            for precedence in [vec![], vec![provider("syntax")], vec![provider("lsp")]] {
                for record in graph.records() {
                    let one = SymbolAssembler::assemble(&graph, record, &precedence);
                    let from =
                        SymbolAssembler::assemble_from(&graph, &sources, record, &precedence);
                    assert_eq!(format!("{from:?}"), format!("{one:?}"));
                }
            }
        }
        let graph = linked_graph();
        let sources = AssemblySources::new(&graph);
        assert!(
            graph.references().len() >= 5,
            "the fixture supplies references"
        );
        assert!(graph.edges().len() >= 5, "the fixture supplies edges");
        let (mut references, mut edges) = (0, 0);
        for record in graph.records() {
            let assembled =
                SymbolAssembler::assemble_from(&graph, &sources, record, &[]).expect("assembly");
            references += assembled.references().len();
            edges += assembled.edges().len();
        }
        assert_eq!(references, graph.references().len());
        assert_eq!(edges, graph.edges().len());
    }

    #[test]
    fn assembles_facts_answers_what_assembly_answers_without_assembling() {
        let graph = graph();
        for record in graph.records() {
            let assembled = SymbolAssembler::assemble(&graph, record, &[provider("syntax")]);
            assert_eq!(
                SymbolAssembler::assembles_facts(&graph, record),
                assembled
                    .as_ref()
                    .and_then(super::AssembledSymbol::facts)
                    .is_some()
            );
        }
        let beacon = reference("syntax", "syntax-beacon");
        let position = graph.record_position_for(&beacon).expect("record position");
        assert_eq!(graph.records().get(position), graph.record_for(&beacon));
        let record = &graph.records()[position];
        assert!(SymbolAssembler::assembles_facts(&graph, record));
        let stale = rift_core::SymbolRecord::new(
            IndexRevision::new(2).expect("index"),
            record.identity().cloned(),
            record.resolution(),
            record.contributions().to_vec(),
        )
        .expect("stale record");
        assert!(SymbolAssembler::assemble(&graph, &stale, &[]).is_none());
        assert!(!SymbolAssembler::assembles_facts(&graph, &stale));
        let missing = rift_core::SymbolRecord::new(
            record.index_revision(),
            None,
            rift_core::SymbolResolution::Unresolved,
            vec![ContributionKey::new(
                provider("syntax"),
                ProviderRevision::new(9).expect("revision"),
                ProviderSymbolId::new("absent").expect("provider symbol"),
            )],
        )
        .expect("record with an uncaptured contribution");
        assert!(SymbolAssembler::assemble(&graph, &missing, &[]).is_none());
        assert!(!SymbolAssembler::assembles_facts(&graph, &missing));
    }

    #[test]
    fn distinct_facets_allow_different_declaration_fields_but_one_facet_does_not() {
        let identity = "rift://symbol/local/rust/app/Beacon";
        let primary = contribution("syntax", 1, "declaration", "Beacon", Some(identity), None);
        let selected = primary.facts().expect("primary facts");
        let unit = primary.source().expect("source binding").unit().clone();
        for (start, separate) in [(0, false), (20, true)] {
            let facts = PortableSymbolFacts::new(
                selected.language().clone(),
                selected.name(),
                selected.qualified_name(),
                ExactKind("impl_item".to_owned()),
            )
            .visibility("pub");
            let alternate = Contribution::builder(
                ContributionKey::new(
                    provider("syntax"),
                    ProviderRevision::new(1).expect("revision"),
                    ProviderSymbolId::new("implementation").expect("provider symbol"),
                ),
                primary.applicability(),
                facts,
                primary.origin().clone(),
            )
            .source(rift_core::DeclarationBinding::new(
                unit.clone(),
                SourceRange::new(start, start + 8).expect("declaration range"),
            ))
            .identity_anchor(SymbolId::new(identity).expect("identity"))
            .build()
            .expect("alternate contribution");
            let contributions = [(alternate.key().clone(), &alternate)];
            let differences =
                super::presentation_disagreements(&contributions, &primary, selected, None, None);
            assert_eq!(differences.is_empty(), separate, "{differences:?}");
        }
    }

    fn facet(
        symbol: &str,
        (kind, file): (&str, &str),
        identity: &str,
        container: Option<&str>,
        visibility: Option<&str>,
    ) -> Contribution {
        let mut facts = PortableSymbolFacts::new(
            Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            symbol,
            symbol,
            ExactKind(kind.to_owned()),
        );
        if kind == "struct" {
            facts = facts.kinds(vec![SymbolKind::Type]);
        }
        if let Some(container) = container {
            facts = facts.container(reference("syntax", container));
        }
        if let Some(visibility) = visibility {
            facts = facts.visibility(visibility);
        }
        let start = symbol.bytes().map(u64::from).sum();
        Contribution::builder(
            ContributionKey::new(
                provider("syntax"),
                ProviderRevision::new(1).expect("revision"),
                ProviderSymbolId::new(symbol).expect("provider symbol"),
            ),
            SourceApplicability::Exact {
                source_revision: SourceRevision::new(1).expect("source"),
                tree_revision: TreeRevision::new(1).expect("tree"),
            },
            facts,
            ContributionOrigin::new(
                Some(SourceLocation::Project { package: None }),
                SourceKind::Authored,
            )
            .expect("origin"),
        )
        .source(DeclarationBinding::new(
            SourceUnitId::parse(&format!("rift://source/rift.sources.project/src/{file}.rs"))
                .expect("unit"),
            SourceRange::new(start, start + 1).expect("range"),
        ))
        .identity_anchor(SymbolId::new(identity).expect("identity"))
        .build()
        .expect("contribution")
    }

    /// Within one provider a definition precedes the implementations in its file, and those
    /// precede the ones in another file, whatever their keys; none fills a field a lower rank
    /// leaves out. A type without a definition keeps the order of the keys.
    #[test]
    fn a_definition_gives_the_fields_whatever_its_key_sorts_after() {
        let task = "rift://symbol/local/rust/app/Task";
        let size = "rift://symbol/local/rust/app/Task/size";
        let go = "rift://symbol/local/rust/app/Task/go";
        let module = "rift://symbol/local/rust/app/inner";
        let other = "rift://symbol/local/rust/app/Other";
        let publication = ProviderPublication::new(
            provider("syntax"),
            ProviderRevision::new(1).expect("revision"),
            vec![
                facet("module", ("module", "facade"), module, None, None),
                facet(
                    "a-block",
                    ("impl_item", "facade"),
                    task,
                    Some("module"),
                    Some("private"),
                ),
                facet(
                    "b-block",
                    ("impl_item", "task"),
                    task,
                    None,
                    Some("private"),
                ),
                facet("task", ("struct", "task"), task, None, None),
                facet(
                    "a-size",
                    ("function", "facade"),
                    size,
                    Some("a-block"),
                    Some("private"),
                ),
                facet(
                    "size",
                    ("field_declaration", "task"),
                    size,
                    Some("task"),
                    None,
                ),
                facet(
                    "a-go",
                    ("function", "facade"),
                    go,
                    Some("a-block"),
                    Some("pub"),
                ),
                facet("b-go", ("function", "task"), go, Some("b-block"), None),
                facet(
                    "a-other",
                    ("impl_item", "facade"),
                    other,
                    Some("module"),
                    Some("private"),
                ),
                facet("other", ("function", "task"), other, None, Some("pub")),
            ],
            PublicationLimits::default(),
        )
        .expect("publication");
        let set = PublicationSet::empty(PublicationLimits::default())
            .replaced(publication)
            .expect("set");
        let graph = Normalizer::normalize(
            IndexRevision::new(1).expect("index"),
            SourceRevision::new(1).expect("source"),
            TreeRevision::new(1).expect("tree"),
            &Arc::new(set),
            None,
        )
        .expect("graph");
        for (order, container, visibility) in [
            (&["task", "b-block", "a-block"][..], None, None),
            (&["size", "a-size"][..], Some(task), None),
            (&["b-go", "a-go"][..], Some(task), None),
            (&["a-other", "other"][..], Some(module), Some("private")),
        ] {
            let record = graph
                .record_for(&reference("syntax", order[0]))
                .expect("record");
            let assembled =
                SymbolAssembler::assemble(&graph, record, &[provider("syntax")]).expect("symbol");
            let facts = assembled.facts().expect("facts");
            assert_eq!(facts.name(), order[0]);
            assert_eq!(
                assembled
                    .contributions()
                    .iter()
                    .map(|key| key.reference().symbol().as_str())
                    .collect::<Vec<_>>(),
                order
            );
            assert_eq!(assembled.container().map(SymbolId::as_str), container);
            assert_eq!(facts.visibility_spelling(), visibility);
        }
    }

    #[test]
    fn assembly_preserves_identity_and_applies_presentation_precedence() {
        let graph = graph();
        let record = graph
            .records()
            .iter()
            .find(|record| record.identity().is_some())
            .expect("established record");
        let lsp_first = SymbolAssembler::assemble(&graph, record, &[provider("lsp")])
            .expect("assembled symbol");
        assert_eq!(
            lsp_first.identity().map(SymbolId::as_str),
            Some("rift://symbol/local/rust/app/Beacon")
        );
        assert_eq!(lsp_first.facts().expect("portable facts").name(), "beacon");
        assert_eq!(
            lsp_first
                .facts()
                .expect("portable facts")
                .documentation_blocks()
                .len(),
            2
        );
        assert!(lsp_first.container().is_none());
        assert!(
            lsp_first
                .disagreements()
                .iter()
                .any(|value| value.field() == PresentationField::Name)
        );
        assert_eq!(lsp_first.namespaced().len(), 2);

        let syntax_first = SymbolAssembler::assemble(&graph, record, &[provider("syntax")])
            .expect("assembled symbol");
        assert_eq!(
            syntax_first.facts().expect("portable facts").name(),
            "Beacon"
        );
        assert_eq!(syntax_first.identity(), lsp_first.identity());
        assert_eq!(syntax_first.index_revision(), graph.index_revision());
        assert_eq!(syntax_first.resolution(), record.resolution());
        assert_eq!(syntax_first.contributions().len(), 2);
        assert_eq!(
            syntax_first.origin(),
            record
                .contributions()
                .first()
                .and_then(|key| graph.contribution(key))
                .expect("contribution")
                .origin()
        );
    }

    #[test]
    fn protocol_symbol_preserves_extensions_and_external_origin() {
        let graph = graph();
        let record = graph
            .records()
            .iter()
            .find(|record| record.identity().is_some())
            .expect("established record");
        let mut assembled =
            SymbolAssembler::assemble(&graph, record, &[]).expect("assembled symbol");
        let key = rift_core::ExtensionKey("org.rift.history".to_owned());
        let value = rift_core::ExtensionValue {
            version: 1,
            data: serde_json::json!({"commit": "abc123"}),
        };
        assembled.namespaced.push((
            provider("git"),
            Extensions(BTreeMap::from([(key.clone(), value.clone())])),
        ));
        assembled.origin =
            ContributionOrigin::new(Some(SourceLocation::External {}), SourceKind::Authored)
                .expect("external origin");

        let symbol = assembled.to_protocol_symbol(assembled.facts().expect("portable facts"));

        assert_eq!(symbol.extensions.0.get(&key), Some(&value));
        assert_eq!(
            symbol.origin.location,
            Some(rift_protocol::read::SourceLocationKind::External)
        );
        assert!(symbol.origin.package.is_none());
    }

    #[test]
    fn assembly_retains_namespaced_fact_without_portable_fields() {
        let namespaced = Extensions(BTreeMap::from([(
            rift_core::ExtensionKey("org.rift.history".to_owned()),
            rift_core::ExtensionValue {
                version: 1,
                data: serde_json::json!({"commit": "abc123"}),
            },
        )]));
        let fact = Contribution::fact_builder(
            ContributionKey::new(
                provider("git"),
                ProviderRevision::new(1).expect("revision"),
                ProviderSymbolId::new("abc123").expect("provider symbol"),
            ),
            SourceApplicability::Independent,
            ContributionOrigin::new(None, SourceKind::Synthetic).expect("origin"),
        )
        .namespaced(namespaced.clone())
        .build()
        .expect("namespaced fact");
        let limits = PublicationLimits::default();
        let publication = ProviderPublication::new(
            provider("git"),
            ProviderRevision::new(1).expect("revision"),
            vec![fact],
            limits,
        )
        .expect("publication");
        let set = Arc::new(
            PublicationSet::empty(limits)
                .replaced(publication)
                .expect("publication set"),
        );
        let graph = Normalizer::normalize(
            IndexRevision::new(1).expect("index"),
            SourceRevision::new(1).expect("source"),
            TreeRevision::new(1).expect("tree"),
            &set,
            None,
        )
        .expect("graph");
        let assembled =
            SymbolAssembler::assemble(&graph, &graph.records()[0], &[]).expect("assembled fact");

        assert!(assembled.facts().is_none());
        assert_eq!(assembled.namespaced(), &[(provider("git"), namespaced)]);
        let bytes = get_size2::GetSize::get_heap_size(&assembled);
        assert!(bytes > 0 && bytes < usize::MAX);
        assert_eq!(assembled.clone(), assembled);
    }

    #[test]
    fn assembly_refuses_record_from_another_index_revision() {
        let graph = graph();
        let record = rift_core::SymbolRecord::new(
            IndexRevision::new(2).expect("index"),
            Some(SymbolId::new("symbol:other").expect("identity")),
            rift_core::SymbolResolution::Established,
            vec![graph.records()[0].contributions()[0].clone()],
        )
        .expect("record");
        assert!(SymbolAssembler::assemble(&graph, &record, &[]).is_none());
    }
}
