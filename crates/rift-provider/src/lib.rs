//! Provider and source catalog contracts.

mod adapter;
mod assembly;
mod fact;

pub use adapter::{
    AdapterError, AdapterPublication, AdapterViolation, ProviderInputMode, PublicationCoverage,
};
pub use assembly::{AssembledSymbol, PresentationDisagreement, PresentationField, SymbolAssembler};
pub use fact::{DirectFactAdapter, DirectFactError, DirectFactInput, DirectFactViolation};

mod composition;
mod normalization;
mod publication;

pub use composition::{
    CacheUpdate, Component, CompositionBuilder, CompositionEditor, CompositionScope, Flow,
    FlowCardinality, JoinCoverage, JoinItem, JoinSides, KeyJoinPolicy, KeyedFlow, MissingSide,
    PerKeyCache, ProviderComposition, StageDescriptor, StagePath, join_keyed,
};
pub use normalization::{
    AssociationCandidate, AssociationState, NormalizedGraph, NormalizedReference,
    NormalizedRelationship, NormalizedTarget, Normalizer, Scope,
};
pub use publication::{
    CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT, CONTRIBUTIONS_TOTAL_MAX_DEFAULT, PROVIDERS_MAX_DEFAULT,
    ProviderPublication, PublicationLimits, PublicationSet, PublicationStore,
};

/// Compile-time marker for provider-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderLayer;
