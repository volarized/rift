//! Provider and source catalog contracts.

mod assembly;

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
pub use normalization::{AssociationCandidate, AssociationState, NormalizedGraph, Normalizer};
pub use publication::{
    CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT, CONTRIBUTIONS_TOTAL_MAX_DEFAULT, PROVIDERS_MAX_DEFAULT,
    ProviderPublication, PublicationError, PublicationFault, PublicationLimits, PublicationSet,
    PublicationStore, PublicationViolation,
};

/// Compile-time marker for provider-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderLayer;
