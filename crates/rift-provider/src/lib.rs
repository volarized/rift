//! Provider and source catalog contracts.

mod composition;

pub use composition::{
    Component, CompositionBuilder, CompositionEditor, CompositionError, CompositionErrorKind,
    CompositionScope, Flow, FlowCardinality, ProviderComposition, StageDescriptor, StagePath,
};

/// Compile-time marker for provider-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderLayer;
