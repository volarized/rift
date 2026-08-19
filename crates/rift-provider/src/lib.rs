//! Provider and source catalog contracts.

mod composition;

pub use composition::{
    CacheError, CacheUpdate, CacheViolation, Component, CompositionBuilder, CompositionEditor,
    CompositionError, CompositionErrorKind, CompositionScope, Flow, FlowCardinality, JoinCoverage,
    JoinItem, KeyJoinPolicy, KeyedFlow, MissingSide, PerKeyCache, ProviderComposition,
    StageDescriptor, StagePath, join_keyed,
};

/// Compile-time marker for provider-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderLayer;
