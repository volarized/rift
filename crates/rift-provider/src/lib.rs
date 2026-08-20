//! Provider and source catalog contracts.

mod composition;

pub use composition::{
    CacheError, CacheFault, CacheUpdate, CacheViolation, Component, CompositionBuilder,
    CompositionEditor, CompositionError, CompositionFault, CompositionScope, Flow, FlowCardinality,
    JoinCoverage, JoinItem, JoinSides, KeyJoinPolicy, KeyedFlow, MissingSide, PerKeyCache,
    ProviderComposition, StageDescriptor, StagePath, join_keyed,
};

/// Compile-time marker for provider-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderLayer;
