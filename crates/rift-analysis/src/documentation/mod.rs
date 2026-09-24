//! Bounded documentation collection over source bytes selected by a caller.

#[cfg(feature = "collector")]
mod collect;
#[cfg(all(test, feature = "collector"))]
mod collect_tests;
mod context;
#[cfg(all(test, feature = "collector"))]
mod coverage_tests;
mod failure;
mod identity;
mod input;
mod links;
#[cfg(feature = "collector")]
pub mod notebook;
pub mod projection;
mod publication;
mod references;
#[cfg(feature = "collector")]
mod resolution;
#[cfg(feature = "collector")]
mod rst;

pub use failure::{DocumentationError, DocumentationFault, DocumentationViolation};
pub use input::{DocumentationInput, DocumentationSourceSet, check_documentation_source_count};
pub use links::{DocumentationFragment, linked_blocks, resolve_links};
pub use projection::{
    DocumentationLayer, DocumentationProjection, DocumentationProjectionTarget, LAYER_BLOCKS_MAX,
    LAYER_MAPPINGS_MAX,
};
pub use publication::{
    DocumentationChanges, DocumentationCollection, DocumentationLinkChanges,
    DocumentationLinkReplacement, DocumentationRecordChanges, DocumentationSourceChanges,
    validate_documentation_context, validate_documentation_hit,
};
pub use references::{
    DocumentationDeclaration, ResolvedDocumentationReferences, resolve_references,
};

pub use crate::analyzer_revision as documentation_revision;
#[cfg(feature = "collector")]
pub use collect::{collect_documentation, collect_documentation_incremental};
pub use context::{documentation_context, documentation_context_with_budget};
pub use identity::{content_chunk_identity, content_digest, content_owner_identity};
