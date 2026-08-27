//! Provider and source catalog contracts.

mod adapter;
mod assembly;
mod scip;
mod stack_graph;

pub use adapter::{
    AdapterError, AdapterPublication, AdapterViolation, ProviderInputMode, PublicationCoverage,
};
pub use assembly::{AssembledSymbol, PresentationDisagreement, PresentationField, SymbolAssembler};

mod composition;
mod normalization;
mod publication;

pub use composition::{
    CacheError, CacheFault, CacheUpdate, CacheViolation, Component, CompositionBuilder,
    CompositionEditor, CompositionError, CompositionFault, CompositionScope, Flow, FlowCardinality,
    JoinCoverage, JoinItem, JoinSides, KeyJoinPolicy, KeyedFlow, MissingSide, PerKeyCache,
    ProviderComposition, StageDescriptor, StagePath, join_keyed,
};
pub use normalization::{
    AssociationCandidate, AssociationState, NormalizedGraph, NormalizedReference,
    NormalizedRelationship, NormalizedTarget, Normalizer, Scope,
};
pub use publication::{
    CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT, CONTRIBUTIONS_TOTAL_MAX_DEFAULT, PROVIDERS_MAX_DEFAULT,
    ProviderPublication, PublicationError, PublicationFault, PublicationLimits, PublicationSet,
    PublicationStore, PublicationViolation,
};
pub use scip::{ScipAdapter, ScipAdapterError, ScipViolation};
pub use stack_graph::{
    StackGraphAdapter, StackGraphAdapterError, StackGraphDefinition, StackGraphInput,
    StackGraphReference, StackGraphScope, StackGraphViolation,
};

/// Compile-time marker for provider-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderLayer;
