//! Direct fact Contribution adapter.

use std::fmt;

use rift_core::{
    Contribution, ContributionKey, ContributionOrigin, ContributionRelationship,
    DeclarationBinding, EquivalenceEvidence, Extensions, PortableSymbolFacts, ProviderId,
    ProviderRevision, ProviderSymbolId, SemanticReference, SourceApplicability,
};

use crate::{
    AdapterPublication, ProviderInputMode, ProviderPublication, PublicationCoverage,
    PublicationLimits,
};

/// One directly published provider fact.
#[derive(Debug, Clone)]
pub struct DirectFactInput {
    symbol: ProviderSymbolId,
    applicability: SourceApplicability,
    origin: ContributionOrigin,
    portable: Option<PortableSymbolFacts>,
    source: Option<DeclarationBinding>,
    equivalence: Vec<EquivalenceEvidence>,
    references: Vec<SemanticReference>,
    relationships: Vec<ContributionRelationship>,
    namespaced: Extensions,
}

impl DirectFactInput {
    /// Creates one provider fact without portable fields.
    #[must_use]
    pub fn new(
        symbol: ProviderSymbolId,
        applicability: SourceApplicability,
        origin: ContributionOrigin,
        namespaced: Extensions,
    ) -> Self {
        Self {
            symbol,
            applicability,
            origin,
            portable: None,
            source: None,
            equivalence: Vec::new(),
            references: Vec::new(),
            relationships: Vec::new(),
            namespaced,
        }
    }

    /// Sets portable fields used by common queries.
    #[must_use]
    pub fn portable(mut self, portable: PortableSymbolFacts) -> Self {
        self.portable = Some(portable);
        self
    }

    /// Sets exact declaration coordinates.
    #[must_use]
    pub fn source(mut self, source: DeclarationBinding) -> Self {
        self.source = Some(source);
        self
    }

    /// Sets association evidence.
    #[must_use]
    pub fn equivalence(mut self, equivalence: Vec<EquivalenceEvidence>) -> Self {
        self.equivalence = equivalence;
        self
    }

    /// Sets semantic References.
    #[must_use]
    pub fn references(mut self, references: Vec<SemanticReference>) -> Self {
        self.references = references;
        self
    }

    /// Sets portable Relationships.
    #[must_use]
    pub fn relationships(mut self, relationships: Vec<ContributionRelationship>) -> Self {
        self.relationships = relationships;
        self
    }

    fn into_contribution(
        self,
        provider: ProviderId,
        revision: ProviderRevision,
    ) -> Result<Contribution, DirectFactError> {
        let key = ContributionKey::new(provider, revision, self.symbol);
        let mut builder = match self.portable {
            Some(portable) => Contribution::builder(key, self.applicability, portable, self.origin),
            None => Contribution::fact_builder(key, self.applicability, self.origin),
        };
        if let Some(source) = self.source {
            builder = builder.source(source);
        }
        builder
            .equivalence(self.equivalence)
            .references(self.references)
            .relationships(self.relationships)
            .namespaced(self.namespaced)
            .build()
            .map_err(|error| {
                direct_fact_error(DirectFactViolation::InvalidContribution, error.to_string())
            })
    }
}

/// Converts directly published facts into one atomic publication.
#[derive(Debug, Clone)]
pub struct DirectFactAdapter {
    provider: ProviderId,
    revision: ProviderRevision,
    coverage: PublicationCoverage,
    limits: PublicationLimits,
}

impl DirectFactAdapter {
    /// Creates one direct fact adapter.
    #[must_use]
    pub const fn new(
        provider: ProviderId,
        revision: ProviderRevision,
        coverage: PublicationCoverage,
        limits: PublicationLimits,
    ) -> Self {
        Self {
            provider,
            revision,
            coverage,
            limits,
        }
    }

    /// Publishes one fact collection atomically.
    ///
    /// # Errors
    ///
    /// Returns [`DirectFactError`] when a Contribution, publication, or coverage is invalid.
    pub fn publish(
        &self,
        inputs: Vec<DirectFactInput>,
    ) -> Result<AdapterPublication, DirectFactError> {
        let contributions = inputs
            .into_iter()
            .map(|input| input.into_contribution(self.provider.clone(), self.revision))
            .collect::<Result<Vec<_>, _>>()?;
        let publication = ProviderPublication::new(
            self.provider.clone(),
            self.revision,
            contributions,
            self.limits,
        )
        .map_err(|error| {
            direct_fact_error(DirectFactViolation::InvalidPublication, error.to_string())
        })?;
        AdapterPublication::new(ProviderInputMode::Fact, self.coverage.clone(), publication)
            .map_err(|error| {
                direct_fact_error(DirectFactViolation::InvalidCoverage, error.to_string())
            })
    }
}

/// Stable direct fact conversion failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectFactViolation {
    /// Contribution validation failed.
    InvalidContribution,
    /// Provider publication validation failed.
    InvalidPublication,
    /// Publication coverage validation failed.
    InvalidCoverage,
}

/// Error returned by direct fact conversion.
#[derive(Debug)]
pub struct DirectFactError {
    violation: DirectFactViolation,
    detail: String,
}

impl DirectFactError {
    /// Returns stable violation.
    #[must_use]
    pub const fn violation(&self) -> DirectFactViolation {
        self.violation
    }

    /// Returns failure detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for DirectFactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "direct fact adapter rejected {:?}: {}",
            self.violation, self.detail
        )
    }
}

impl std::error::Error for DirectFactError {}

fn direct_fact_error(violation: DirectFactViolation, detail: impl Into<String>) -> DirectFactError {
    DirectFactError {
        violation,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use rift_core::{
        ContributionOrigin, ContributionReference, ContributionRelationship, DeclarationBinding,
        EquivalenceEvidence, ExactKind, ExtensionKey, ExtensionValue, Extensions, IndexRevision,
        Language, PortableSymbolFacts, ProviderId, ProviderRevision, ProviderSymbolId,
        ReferenceRole, RelationshipKind, SemanticReference, SourceApplicability, SourceKind,
        SourceLocation, SourcePath, SourceRange, SourceResolverId, SourceRevision, SourceUnitId,
        SymbolResolution, TreeRevision,
    };
    use serde_json::json;

    use super::{DirectFactAdapter, DirectFactInput, DirectFactViolation};
    use crate::{
        Normalizer, ProviderInputMode, PublicationCoverage, PublicationLimits, PublicationSet,
    };

    fn provider() -> ProviderId {
        ProviderId::new("direct-facts").expect("provider")
    }

    fn revision() -> ProviderRevision {
        ProviderRevision::new(3).expect("revision")
    }

    fn exact() -> SourceApplicability {
        SourceApplicability::Exact {
            source_revision: SourceRevision::new(4).expect("source revision"),
            tree_revision: TreeRevision::new(5).expect("tree revision"),
        }
    }

    fn synthetic_origin() -> ContributionOrigin {
        ContributionOrigin::new(None, SourceKind::Synthetic).expect("origin")
    }

    fn project_origin(kind: SourceKind) -> ContributionOrigin {
        ContributionOrigin::new(Some(SourceLocation::Project { package: None }), kind)
            .expect("origin")
    }

    fn namespaced(key: &str, data: serde_json::Value) -> Extensions {
        Extensions(BTreeMap::from([(
            ExtensionKey(key.to_owned()),
            ExtensionValue { version: 1, data },
        )]))
    }

    fn binding(path: &str, start: u64, end: u64) -> DeclarationBinding {
        DeclarationBinding::new(
            SourceUnitId::new(
                SourceResolverId::new("project").expect("resolver"),
                SourcePath::new(path).expect("path"),
            )
            .expect("unit"),
            SourceRange::new(start, end).expect("range"),
            None,
        )
    }

    #[test]
    fn framework_and_documentation_facts_publish_unresolved() {
        let framework = DirectFactInput::new(
            ProviderSymbolId::new("framework:route").expect("symbol"),
            SourceApplicability::Independent,
            synthetic_origin(),
            namespaced("org.example.framework", json!({"route": "/beacon"})),
        );
        let documentation = DirectFactInput::new(
            ProviderSymbolId::new("docs:guide").expect("symbol"),
            SourceApplicability::Independent,
            synthetic_origin(),
            namespaced("org.example.documentation", json!({"section": "Beacon"})),
        );
        let output = DirectFactAdapter::new(
            provider(),
            revision(),
            PublicationCoverage::Workspace,
            PublicationLimits::default(),
        )
        .publish(vec![framework, documentation])
        .expect("fact publication");

        assert_eq!(output.mode(), ProviderInputMode::Fact);
        assert_eq!(output.coverage(), &PublicationCoverage::Workspace);
        assert!(
            output
                .publication()
                .contributions()
                .iter()
                .all(|contribution| {
                    contribution.facts().is_none()
                        && contribution.equivalence().is_empty()
                        && contribution.identity_anchor().is_none()
                })
        );
        let limits = PublicationLimits::default();
        let publications = Arc::new(
            PublicationSet::empty(limits)
                .replaced(output.into_publication())
                .expect("publication set"),
        );
        let graph = Normalizer::normalize(
            IndexRevision::new(1).expect("index revision"),
            SourceRevision::new(1).expect("source revision"),
            TreeRevision::new(1).expect("tree revision"),
            &publications,
            None,
        )
        .expect("normalized graph");

        assert_eq!(graph.records().len(), 2);
        assert!(graph.records().iter().all(|record| {
            record.resolution() == SymbolResolution::Unresolved && record.identity().is_none()
        }));
    }

    #[test]
    fn generated_source_fact_keeps_generator_and_source_mapping() {
        let output_binding = binding("target/generated.rs", 0, 12);
        let portable = PortableSymbolFacts::new(
            Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            "Beacon",
            "generated::Beacon",
            ExactKind("rust.struct".to_owned()),
        );
        let target = ContributionReference::new(
            ProviderId::new("syntax").expect("target provider"),
            ProviderSymbolId::new("Beacon").expect("target symbol"),
        );
        let reference = SemanticReference::new(
            binding("target/generated.rs", 20, 26),
            ReferenceRole::Call,
            vec![target.clone()],
        )
        .expect("reference");
        let relationship = ContributionRelationship::new(RelationshipKind::Implementation, target);
        let generated = DirectFactInput::new(
            ProviderSymbolId::new("generated:Beacon").expect("symbol"),
            exact(),
            project_origin(SourceKind::Generated),
            namespaced(
                "org.rift.generated_source",
                json!({
                    "generator": "build-script",
                    "source_mapping": {
                        "source": "schema/beacon.json",
                        "range": {"start": 4, "end": 10}
                    }
                }),
            ),
        )
        .portable(portable)
        .source(output_binding.clone())
        .equivalence(vec![EquivalenceEvidence::Declaration(output_binding)])
        .references(vec![reference])
        .relationships(vec![relationship]);
        let output = DirectFactAdapter::new(
            provider(),
            revision(),
            PublicationCoverage::Workspace,
            PublicationLimits::default(),
        )
        .publish(vec![generated])
        .expect("generated publication");
        let contribution = &output.publication().contributions()[0];

        assert_eq!(contribution.applicability(), exact());
        assert_eq!(contribution.references()[0].role(), ReferenceRole::Call);
        assert_eq!(
            contribution.relationships()[0].kind(),
            RelationshipKind::Implementation
        );
        let generated = contribution
            .namespaced()
            .0
            .get(&ExtensionKey("org.rift.generated_source".to_owned()))
            .expect("generated fact");
        assert_eq!(generated.data["generator"], "build-script");
        assert_eq!(
            generated.data["source_mapping"]["source"],
            "schema/beacon.json"
        );
    }

    #[test]
    fn invalid_namespaced_fact_reports_contribution_failure() {
        let input = DirectFactInput::new(
            ProviderSymbolId::new("bad").expect("symbol"),
            SourceApplicability::Independent,
            synthetic_origin(),
            namespaced("Bad", json!({"value": true})),
        );
        let error = DirectFactAdapter::new(
            provider(),
            revision(),
            PublicationCoverage::Workspace,
            PublicationLimits::default(),
        )
        .publish(vec![input])
        .expect_err("invalid namespace");

        assert_eq!(error.violation(), DirectFactViolation::InvalidContribution);
        assert!(!error.detail().is_empty());
        assert!(!error.to_string().is_empty());
        let _: &dyn std::error::Error = &error;
    }

    #[test]
    fn source_unit_coverage_refuses_fact_outside_declared_units() {
        let input = DirectFactInput::new(
            ProviderSymbolId::new("generated").expect("symbol"),
            exact(),
            project_origin(SourceKind::Generated),
            namespaced("org.rift.generated_source", json!({"generator": "test"})),
        )
        .source(binding("target/generated.rs", 0, 4));
        let error = DirectFactAdapter::new(
            provider(),
            revision(),
            PublicationCoverage::SourceUnits(vec![binding("src/lib.rs", 0, 1).unit().clone()]),
            PublicationLimits::default(),
        )
        .publish(vec![input])
        .expect_err("outside coverage");

        assert_eq!(error.violation(), DirectFactViolation::InvalidCoverage);
    }

    #[test]
    fn duplicate_provider_symbols_report_publication_failure() {
        let input = || {
            DirectFactInput::new(
                ProviderSymbolId::new("duplicate").expect("symbol"),
                SourceApplicability::Independent,
                synthetic_origin(),
                namespaced("org.example.framework", json!({"value": true})),
            )
        };
        let error = DirectFactAdapter::new(
            provider(),
            revision(),
            PublicationCoverage::Workspace,
            PublicationLimits::default(),
        )
        .publish(vec![input(), input()])
        .expect_err("duplicate symbol");

        assert_eq!(error.violation(), DirectFactViolation::InvalidPublication);
    }
}
