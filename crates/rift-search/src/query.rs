//! Query-language-independent domain facts and acquisition requirements.

use rift_protocol::documentation::{
    DocumentationBlock, DocumentationLink, DocumentationReference, DocumentationReferenceCandidate,
    DocumentationSource,
};
use rift_protocol::query::QueryOccurrence;
pub use rift_protocol::query::{QuerySource, QuerySymbol};
use rift_protocol::read::{
    Node, Relationship, RelationshipFacet, SymbolHistory, SymbolId, TraversalDirection,
};
use serde::{Deserialize, Serialize};

/// Complete domain records acquired under one immutable serving selection.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct QueryFacts {
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
    /// Source units and requested bytes.
    pub sources: Vec<QuerySource>,
    /// Source structure from the selected syntax publication.
    pub nodes: Vec<Node>,
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

/// Sufficient domain operations required before evaluation can report a complete answer.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct FactRequirements {
    /// Whether ordinary selected domain facts are required outside a query service.
    pub domain: bool,
    /// Whether the result describes the complete captured published records.
    pub description: bool,
    /// Exact name restriction proved for a single subject's ordinary pattern.
    pub symbol_name: Option<String>,
    /// Exact symbol restriction proved for a physical usage-only pattern.
    pub symbol_ids: Option<Vec<SymbolId>>,
    /// Anchored target identities whose physical usages must be acquired.
    pub occurrences: Vec<SymbolId>,
    /// Required anchored relationship acquisitions.
    pub adjacency: Vec<AdjacencyRequirement>,
    /// Whether the selected semantic domain must be completely acquired.
    pub complete_domain: bool,
    /// Whether source bytes must be available to the mapping.
    pub source: bool,
    /// Whether complete concrete syntax node rows are required.
    pub nodes: bool,
    /// Whether selected symbol timelines are required.
    pub history: bool,
    /// Exact symbols selected through their timeline relationship.
    pub history_symbols: Vec<SymbolId>,
    /// Whether timeline facts are required beyond exact selected symbols.
    pub all_histories: bool,
    /// Whether collected documentation records are required.
    pub documentation: bool,
    /// Whether unanchored physical usages are required.
    pub all_occurrences: bool,
    /// Relationship facets used anywhere in the algebra.
    pub facets: Vec<RelationshipFacet>,
    /// Explicit dataset and named-graph IRIs, resolved by the serving owner.
    pub datasets: Vec<String>,
    /// Explicit service IRIs, resolved by the serving owner.
    pub services: Vec<String>,
    /// Existing retrieval requests executed before language evaluation.
    pub retrievals: Vec<rift_protocol::read::SearchParams>,
}
