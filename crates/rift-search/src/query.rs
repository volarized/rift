//! Query-language-independent domain facts and acquisition requirements.

mod provider;
mod source;

pub use source::{
    qualify_source_unit, require_source_agreement, source_digest, validate_source_selections,
    validate_source_selections_by_unit,
};

pub use provider::{
    QueryProviderContext, QueryProviderDeclaration, QueryProviderEdge, QueryProviderEndpoint,
    QueryProviderInput, QueryProviderRecord, QueryProviderReference, QueryProviderRelationship,
    QueryProviderSelection, QueryProviderSnapshot, QueryProviderTarget, contribution_keys,
};

use std::collections::BTreeSet;

use rift_protocol::documentation::{
    DocumentationBlock, DocumentationLink, DocumentationReference, DocumentationReferenceCandidate,
    DocumentationSource,
};
use rift_protocol::query::QueryOccurrence;
pub use rift_protocol::query::{QuerySource, QuerySymbol};
use rift_protocol::read::{
    Relationship, RelationshipFacet, SymbolHistory, SymbolId, TraversalDirection,
};
use serde::{Deserialize, Serialize};

mod nodes;
pub use nodes::{QueryNode, QueryNodeSource};

#[cfg(test)]
pub(crate) use nodes::tests::source as test_node_source;

/// Complete domain records acquired under one immutable serving selection.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryFacts {
    /// Original normalized provider graphs associated with the existing payload arrays.
    pub providers: Vec<QueryProviderSnapshot>,
    /// Engine-established links retaining canonical or supplied physical endpoints.
    pub provider_edges: Vec<QueryProviderEdge>,
    /// Families not captured by the selected publication.
    pub unavailable_families: Vec<QueryFactFamily>,
    /// Established logical symbols and their physical declarations.
    pub symbols: Vec<QuerySymbol>,
    /// Evidence-bearing relationships, including parallel occurrences.
    pub relationships: Vec<Relationship>,
    /// Original selected-publication records, retaining distinct identities for equal payloads.
    pub relationship_records: Vec<rift_protocol::relationship_read::RelationshipRecord>,
    /// Physical usage locations independent of an enclosing declaration.
    pub occurrences: Vec<QueryOccurrence>,
    /// Original selected-publication usage records and their established relationship links.
    pub occurrence_records: Vec<rift_protocol::relationship_read::RelationshipOccurrence>,
    /// Source units and requested bytes.
    pub sources: Vec<QuerySource>,
    /// Complete original named trees from the selected syntax publication.
    pub nodes: Vec<QueryNodeSource>,
    /// Selected symbol timelines, retaining their completeness decisions.
    pub histories: Vec<SymbolHistory>,
    /// Captured documentation source ownership and revisions.
    pub documentation_sources: Vec<DocumentationSource>,
    /// Captured documentation blocks.
    pub documentation_blocks: Vec<DocumentationBlock>,
    /// Authored documentation links, including unresolved targets.
    pub documentation_links: Vec<DocumentationLink>,
    /// Established documentation references and their evidence.
    pub documentation_references: Vec<DocumentationReference>,
    /// Authored reference candidates not established as exact references.
    pub documentation_reference_candidates: Vec<DocumentationReferenceCandidate>,
}

/// Domain families whose absence must not become a negative query answer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum QueryFactFamily {
    /// Complete concrete syntax node rows.
    Nodes,
    /// Symbol timelines from the selected history owner.
    History,
    /// Captured documentation collection.
    Documentation,
}

/// Accepted bounds supplied by the serving owner.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QueryLimits {
    /// Most UTF-8 bytes admitted before parsing.
    pub query_bytes_max: usize,
    /// Most algebra nodes inspected during acquisition planning.
    pub algebra_nodes_max: usize,
    /// Most RDF statements projected from the selected facts.
    pub quads_max: usize,
    /// Most bytes retained for RDF terms.
    pub rdf_bytes_max: usize,
    /// Most rows or triples retained from one execution.
    pub result_items_max: usize,
    /// Most serialized result bytes retained from one execution.
    pub result_bytes_max: usize,
    /// Explicit encoded request ceiling, or the accepted size derivation.
    #[serde(deserialize_with = "required_byte_override")]
    pub input_bytes_max: Option<usize>,
    /// Explicit encoded response ceiling, or the accepted size derivation.
    #[serde(deserialize_with = "required_byte_override")]
    pub output_bytes_max: Option<usize>,
    /// Explicit acquired material ceiling, or the accepted size derivation.
    #[serde(deserialize_with = "required_byte_override")]
    pub material_bytes_max: Option<usize>,
    /// Explicit live Rust allocation ceiling, or the accepted size derivation.
    #[serde(deserialize_with = "required_byte_override")]
    pub heap_bytes_max: Option<usize>,
    /// Explicit standard error prefix ceiling, or the diagnostic default.
    #[serde(deserialize_with = "required_byte_override")]
    pub diagnostic_bytes_max: Option<usize>,
}

fn required_byte_override<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<usize>, D::Error> {
    Option::<usize>::deserialize(deserializer)
}

impl TryFrom<&rift_protocol::query::QueryConfiguration> for QueryLimits {
    type Error = rift_error::RiftError;

    fn try_from(
        configuration: &rift_protocol::query::QueryConfiguration,
    ) -> Result<Self, Self::Error> {
        let quantity = |value| {
            usize::try_from(value).map_err(|_| {
                rift_error::errors::server::read_unavailable()
                    .operation("query acquisition")
                    .detail("query configuration exceeds the platform bound")
                    .error()
            })
        };
        Ok(Self {
            query_bytes_max: quantity(configuration.statement_size.bytes())?,
            algebra_nodes_max: quantity(configuration.algebra_nodes)?,
            quads_max: quantity(configuration.statements)?,
            rdf_bytes_max: quantity(configuration.fact_size.bytes())?,
            result_items_max: quantity(configuration.results)?,
            result_bytes_max: quantity(configuration.result_size.bytes())?,
            input_bytes_max: configuration
                .input_size
                .map(|value| quantity(value.bytes()))
                .transpose()?,
            output_bytes_max: configuration
                .output_size
                .map(|value| quantity(value.bytes()))
                .transpose()?,
            material_bytes_max: configuration
                .material_size
                .map(|value| quantity(value.bytes()))
                .transpose()?,
            heap_bytes_max: configuration
                .heap_size
                .map(|value| quantity(value.bytes()))
                .transpose()?,
            diagnostic_bytes_max: configuration
                .diagnostic_size
                .map(|value| quantity(value.bytes()))
                .transpose()?,
        })
    }
}

/// Complete adjacency required from one established subject.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AdjacencyRequirement {
    /// Canonical identity at the anchored end of the expression.
    pub seed: SymbolId,
    /// Direction in which the provider must acquire relationships.
    pub direction: TraversalDirection,
    /// Portable relationships the expression can traverse.
    pub facets: Vec<RelationshipFacet>,
    /// Most successive edges required, or no finite depth for closure.
    pub depth: Option<u32>,
}

/// One domain acquisition required before evaluation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FactRequirement {
    /// Whether ordinary selected domain facts are required outside a query service.
    Domain,
    /// Whether the result describes the complete captured published records.
    Description,
    /// Whether the selected semantic domain must be completely acquired.
    CompleteDomain,
    /// Whether source bytes must be available to the mapping.
    Source,
    /// Whether complete concrete syntax node rows are required.
    Nodes,
    /// Whether selected symbol timelines are required.
    History,
    /// Whether timeline facts are required beyond exact selected symbols.
    AllHistories,
    /// Whether collected documentation records are required.
    Documentation,
    /// Whether unanchored physical usages are required.
    AllOccurrences,
}

/// Sufficient domain operations required before evaluation can report a complete answer.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FactRequirements {
    /// Domain acquisitions required before evaluation.
    pub required: BTreeSet<FactRequirement>,
    /// Exact name restriction proved for a single subject's ordinary pattern.
    pub symbol_name: Option<String>,
    /// Exact symbol restriction proved for a physical usage-only pattern.
    pub symbol_ids: Option<Vec<SymbolId>>,
    /// Exact physical source units proved for every required source and node pattern.
    /// `None` selects the complete source domain when source or node facts are required.
    pub source_units: Option<Vec<rift_protocol::read::SourceUnitId>>,
    /// Anchored target identities whose physical usages must be acquired.
    pub occurrences: Vec<SymbolId>,
    /// Required anchored relationship acquisitions.
    pub adjacency: Vec<AdjacencyRequirement>,
    /// Exact symbols selected through their timeline relationship.
    pub history_symbols: Vec<SymbolId>,
    /// Relationship facets used anywhere in the algebra.
    pub facets: Vec<RelationshipFacet>,
    /// Explicit dataset and named-graph IRIs, resolved by the serving owner.
    pub datasets: Vec<String>,
    /// Explicit service IRIs, resolved by the serving owner.
    pub services: Vec<String>,
    /// Existing retrieval requests executed before language evaluation.
    pub retrievals: Vec<rift_protocol::read::SearchParams>,
    /// Exact source requests executed before language evaluation.
    pub source_reads: Vec<rift_protocol::source_read::GetSourceParams>,
}

impl FactRequirements {
    /// Validates domain selection before request metadata enters acquisition.
    ///
    /// Empty requirements select no domain facts. A requested domain family requires
    /// the domain selection that causes the serving owner to acquire those facts.
    ///
    /// # Errors
    /// Returns an invalid request when a required family has no domain selection.
    pub fn validate_selection(&self) -> Result<(), rift_error::RiftError> {
        if self.source_reads.iter().any(|request| !request.is_valid()) {
            return Err(rift_error::errors::server::read_invalid()
                .field("requirements.source_reads")
                .violation("source selectors or byte range are invalid")
                .error());
        }
        if self.source_units.as_ref().is_some_and(Vec::is_empty) {
            return Err(rift_error::errors::server::read_invalid()
                .field("requirements.source_units")
                .violation("selected source units must not be empty")
                .error());
        }
        if !self.requires(FactRequirement::Domain) && !self.required.is_empty() {
            return Err(rift_error::errors::server::read_invalid()
                .field("requirements")
                .violation("required fact families must include the domain selection")
                .error());
        }
        Ok(())
    }

    /// Returns whether this acquisition is required before evaluation.
    #[must_use]
    pub fn requires(&self, requirement: FactRequirement) -> bool {
        self.required.contains(&requirement)
    }

    /// Requires this domain acquisition before evaluation.
    pub fn require(&mut self, requirement: FactRequirement) {
        self.required.insert(requirement);
    }
}

#[cfg(test)]
mod tests {
    use super::{FactRequirement, FactRequirements, QueryFacts};
    use serde_json::{Value, json};

    #[test]
    fn fact_selection_refuses_families_without_domain() {
        assert!(FactRequirements::default().validate_selection().is_ok());
        let empty_sources = FactRequirements {
            source_units: Some(Vec::new()),
            ..FactRequirements::default()
        };
        assert!(empty_sources.validate_selection().is_err());
        for requirement in [
            FactRequirement::Description,
            FactRequirement::CompleteDomain,
            FactRequirement::Source,
            FactRequirement::Nodes,
            FactRequirement::History,
            FactRequirement::AllHistories,
            FactRequirement::Documentation,
            FactRequirement::AllOccurrences,
        ] {
            let mut requirements = FactRequirements::default();
            requirements.require(requirement);
            let error = requirements
                .validate_selection()
                .expect_err("a required family cannot select an empty domain");
            assert_eq!(error.slug().as_str(), "rift.server.read_invalid");
            requirements.require(FactRequirement::Domain);
            assert!(requirements.validate_selection().is_ok());
        }
    }

    #[test]
    fn requirement_exchange_preserves_every_acquisition() -> Result<(), Box<dyn std::error::Error>>
    {
        let requirements = FactRequirements {
            required: [
                FactRequirement::Domain,
                FactRequirement::Description,
                FactRequirement::CompleteDomain,
                FactRequirement::Source,
                FactRequirement::Nodes,
                FactRequirement::History,
                FactRequirement::AllHistories,
                FactRequirement::Documentation,
                FactRequirement::AllOccurrences,
            ]
            .into(),
            source_units: Some(vec![rift_protocol::read::SourceUnitId::parse(
                "rift://source/project/src/lib.rs",
            )?]),
            ..FactRequirements::default()
        };
        let decoded =
            serde_json::from_value::<FactRequirements>(serde_json::to_value(&requirements)?)?;
        assert_eq!(decoded, requirements);
        Ok(())
    }

    #[test]
    fn requirement_exchange_refuses_old_or_unknown_layout() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut value = serde_json::to_value(FactRequirements::default())?;
        let object = value.as_object_mut().ok_or("requirement object")?;
        object.remove("required");
        object.insert("domain".to_owned(), Value::Bool(true));
        assert!(serde_json::from_value::<FactRequirements>(value).is_err());

        let mut value = serde_json::to_value(FactRequirements::default())?;
        value["required"] = json!(["unknown"]);
        assert!(serde_json::from_value::<FactRequirements>(value).is_err());
        Ok(())
    }

    #[test]
    fn fact_exchange_refuses_unknown_records() -> Result<(), Box<dyn std::error::Error>> {
        let mut value = serde_json::to_value(QueryFacts::default())?;
        value["unknown"] = json!([]);
        assert!(serde_json::from_value::<QueryFacts>(value).is_err());
        Ok(())
    }
}
