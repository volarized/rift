//! Name binding from syntax facts for Rift.
//!
//! A language rule feeds scopes, definitions, references, and links through
//! [`GraphBuilder`]; [`LinkedGraph::link`] joins units through module links, computes the
//! module tree, and attaches member scopes; [`resolve_all`] maps every reference to the
//! definitions it can name under [`BindingLimits`]. [`UnitBindingFacts`] carries one
//! unit's facts under unit-local indices, [`assemble`] joins many units into one
//! graph, and [`BindingPublisher::publish`] turns the resolved graph into one
//! provider publication.

mod failure;
#[cfg(test)]
mod fixture;
mod graph;
mod limits;
mod link;
mod publish;
mod resolve;
mod unit;

pub use failure::{BindingError, BindingFault, BindingViolation};
pub use graph::{
    BindingGraph, Definition, DefinitionId, DefinitionOrder, GraphBuilder, Link, LinkId, LinkKind,
    NAME_BYTES_MAX, NAME_PATH_SEGMENTS_MAX, Name, NamePath, PathAnchor, Rank, Reference,
    ReferenceId, Scope, ScopeId, ScopeKind, Unit, UnitId, Visibility, VisibilitySpelling,
};
pub use limits::{
    BindingLimits, BindingLimitsBuilder, ExhaustedLimit, GRAPH_LINKS_MAX_DEFAULT,
    GRAPH_NODES_MAX_DEFAULT, PATH_DEPTH_MAX_DEFAULT, PUBLICATION_WORK_MAX_DEFAULT,
    REFERENCE_TARGETS_MAX_DEFAULT, REFERENCE_WORK_MAX_DEFAULT, UNIT_DEFINITIONS_MAX_DEFAULT,
    UNIT_LINKS_MAX_DEFAULT, UNIT_REFERENCES_MAX_DEFAULT, UNIT_SCOPES_MAX_DEFAULT,
};
pub use link::LinkedGraph;
pub use publish::{
    BINDING_EXTENSION_KEY, BINDING_EXTENSION_VERSION, BINDING_PROVIDER_ID, BindingPublisher,
};
pub use resolve::{
    CANCELLATION_CHECK_INTERVAL, Cancellation, DEFINITION_RANK, LEXICAL_RANK, NeverCancelled,
    Resolution, ResolutionSet, resolve_all,
};
pub use unit::{
    IMPORT_EXPLICIT_RANK, IMPORT_WILDCARD_RANK, UnitBindingFacts, UnitBindingFactsBuilder,
    UnitDefinition, UnitDefinitionIndex, UnitImport, UnitMemberLink, UnitModuleDeclaration,
    UnitReference, UnitScope, UnitScopeIndex, assemble,
};

/// Compile-time marker for binding-layer ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingLayer;

#[cfg(test)]
mod tests {
    use super::{BindingGraph, BindingLayer, LinkedGraph, ResolutionSet};
    use crate::resolve::Resolver;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn test_binding_types_cross_threads_send_sync() {
        assert_send_sync::<BindingGraph>();
        assert_send_sync::<LinkedGraph<'static>>();
        assert_send_sync::<ResolutionSet>();
        assert_send_sync::<Resolver<'static>>();
        assert_eq!(BindingLayer, BindingLayer);
    }
}
