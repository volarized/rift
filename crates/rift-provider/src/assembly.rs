use std::collections::BTreeSet;

use rift_core::{
    Contribution, ContributionKey, ContributionOrigin, Extensions, IndexRevision,
    PortableSymbolFacts, ProviderId, SymbolId, SymbolRecord, SymbolResolution,
};

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
#[derive(Debug, Clone, PartialEq)]
pub struct AssembledSymbol {
    index_revision: IndexRevision,
    identity: Option<SymbolId>,
    resolution: SymbolResolution,
    contributions: Vec<ContributionKey>,
    facts: PortableSymbolFacts,
    origin: ContributionOrigin,
    container: Option<SymbolId>,
    namespaced: Vec<(ProviderId, Extensions)>,
    disagreements: Vec<PresentationDisagreement>,
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

    /// Returns selected and combined portable facts.
    #[must_use]
    pub const fn facts(&self) -> &PortableSymbolFacts {
        &self.facts
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
}

/// Deterministic readable Symbol assembly.
#[derive(Debug)]
pub struct SymbolAssembler;

impl SymbolAssembler {
    /// Assembles one normalized record.
    ///
    /// Providers named earlier in `precedence` select scalar presentation
    /// fields. Unnamed providers follow in provider identity order. Lists
    /// combine in same order without duplicates. Missing captured
    /// Contributions return `None`.
    #[must_use]
    pub fn assemble(
        graph: &NormalizedGraph,
        record: &SymbolRecord,
        precedence: &[ProviderId],
    ) -> Option<AssembledSymbol> {
        if record.index_revision() != graph.index_revision() {
            return None;
        }
        let mut contributions = record
            .contributions()
            .iter()
            .map(|key| graph.contribution(key).map(|value| (key.clone(), value)))
            .collect::<Option<Vec<_>>>()?;
        contributions.sort_by(|(left_key, _), (right_key, _)| {
            provider_rank(left_key, precedence)
                .cmp(&provider_rank(right_key, precedence))
                .then_with(|| left_key.cmp(right_key))
        });
        let primary = contributions.first()?.1;
        let selected_visibility = contributions
            .iter()
            .find_map(|(_, contribution)| contribution.facts().visibility_spelling());
        let selected_container = contributions
            .iter()
            .find_map(|(_, contribution)| contribution.facts().container_reference());
        let facts = combined_facts(
            &contributions,
            primary,
            selected_visibility,
            selected_container,
        );
        let container =
            selected_container.and_then(|reference| graph.identity_for(reference).cloned());
        let disagreements = presentation_disagreements(
            &contributions,
            primary,
            selected_visibility,
            selected_container,
        );
        let namespaced = contributions
            .iter()
            .map(|(key, contribution)| {
                (
                    key.reference().provider().clone(),
                    contribution.namespaced().clone(),
                )
            })
            .collect();
        Some(AssembledSymbol {
            index_revision: record.index_revision(),
            identity: record.identity().cloned(),
            resolution: record.resolution(),
            contributions: contributions.into_iter().map(|(key, _)| key).collect(),
            facts,
            origin: primary.origin().clone(),
            container,
            namespaced,
            disagreements,
        })
    }
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
    primary: &Contribution,
    visibility: Option<&str>,
    container: Option<&rift_core::ContributionReference>,
) -> PortableSymbolFacts {
    let primary_facts = primary.facts();
    let mut facts = PortableSymbolFacts::new(
        primary_facts.language().clone(),
        primary_facts.name(),
        primary_facts.qualified_name(),
        primary_facts.kind().clone(),
    )
    .facets(unique_values(contributions, |facts| facts.symbol_facets()))
    .modifiers(unique_values(contributions, |facts| facts.modifier_words()))
    .types(unique_values(contributions, |facts| facts.type_bindings()))
    .signatures(unique_values(contributions, |facts| {
        facts.signatures_slice()
    }))
    .documentation(unique_values(contributions, |facts| {
        facts.documentation_blocks()
    }))
    .document_local(primary_facts.is_document_local());
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
        for value in values(contribution.facts()) {
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
    visibility: Option<&str>,
    container: Option<&rift_core::ContributionReference>,
) -> Vec<PresentationDisagreement> {
    let selected = primary.facts();
    let mut disagreements = BTreeSet::new();
    for (key, contribution) in contributions.iter().skip(1) {
        let facts = contribution.facts();
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use rift_core::{
        Contribution, ContributionKey, ContributionOrigin, ContributionReference, ExactKind,
        IndexRevision, PortableSymbolFacts, ProviderId, ProviderRevision, ProviderSymbolId,
        SourceApplicability, SourceKind, SourceLocation, SourceRange, SourceRevision, SourceUnitId,
        SymbolFacet, SymbolId, TreeRevision,
    };
    use rift_core::{Documentation, DocumentationFormat, Extensions, Language};

    use super::{PresentationField, SymbolAssembler};
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
            ExactKind("rust.function".to_owned()),
        )
        .facets(vec![SymbolFacet::Value])
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
                    None,
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
            Some("symbol:beacon"),
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
            lsp.facts().clone(),
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
            Some("symbol:beacon")
        );
        assert_eq!(lsp_first.facts().name(), "beacon");
        assert_eq!(lsp_first.facts().documentation_blocks().len(), 2);
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
        assert_eq!(syntax_first.facts().name(), "Beacon");
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
