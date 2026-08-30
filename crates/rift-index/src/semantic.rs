use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use rift_binding::{
    BindingError, BindingLimits, BindingPublisher, LinkedGraph, NeverCancelled, UnitBindingFacts,
    assemble, resolve_all,
};
use rift_core::{
    ContributionError, ContributionOrigin, ContributionReference, IndexRevision, ProviderId,
    ProviderRevision, ProviderSymbolId, RevisionError, SourceKind, SourceLocation, SourceRevision,
    SourceUnitId, SourceUnitIdError, TreeRevision,
};
use rift_protocol::configuration::BindingConfiguration;
use rift_provider::{
    AssembledSymbol, NormalizedGraph, Normalizer, PublicationError, PublicationLimits,
    PublicationSet, SymbolAssembler,
};
use rift_syntax::{
    SYNTAX_PROVIDER_ID, SyntaxDocument, SyntaxPublicationBuilder, SyntaxPublicationError,
    source_unit,
};

/// Whether name binding runs during an index build, and under which bounds.
///
/// The default policy runs the binding provider under [`BindingLimits::default`];
/// the accepted `[providers.binding]` table replaces both halves through
/// [`BindingPolicy::from`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindingPolicy {
    enabled: bool,
    limits: BindingLimits,
}

impl BindingPolicy {
    /// Composes a policy from an explicit switch and bounds.
    #[must_use]
    pub const fn new(enabled: bool, limits: BindingLimits) -> Self {
        Self { enabled, limits }
    }

    /// Whether the binding provider runs at all.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The bounds every binding phase runs under.
    #[must_use]
    pub const fn limits(&self) -> &BindingLimits {
        &self.limits
    }
}

impl Default for BindingPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            limits: BindingLimits::default(),
        }
    }
}

impl From<&BindingConfiguration> for BindingPolicy {
    fn from(configuration: &BindingConfiguration) -> Self {
        let limits = BindingLimits::builder()
            .unit_scopes_max(accepted_bound(configuration.max_unit_scopes))
            .unit_definitions_max(accepted_bound(configuration.max_unit_definitions))
            .unit_references_max(accepted_bound(configuration.max_unit_references))
            .unit_links_max(accepted_bound(configuration.max_unit_links))
            .graph_nodes_max(accepted_bound(configuration.max_graph_nodes))
            .graph_links_max(accepted_bound(configuration.max_graph_links))
            .reference_work_max(accepted_bound(configuration.max_reference_work))
            .path_depth_max(accepted_bound(configuration.max_path_depth))
            .reference_targets_max(accepted_bound(configuration.max_reference_targets))
            .publication_work_max(accepted_bound(configuration.max_publication_work))
            .build()
            .unwrap_or_else(|error| {
                unreachable!(
                    "accepted_bound keeps every bound at least 1, the only value the \
                     builder refuses: {error}"
                )
            });
        Self {
            enabled: configuration.enabled,
            limits,
        }
    }
}

/// One accepted `u64` bound as the in-memory `usize` the binding phases take.
///
/// Configuration acceptance refuses zero and every value above its advertised
/// ceiling, so the floor here only keeps the conversion total for a value built
/// outside acceptance.
fn accepted_bound(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX).max(1)
}

/// Contribution graph captured by one workspace index publication.
#[derive(Debug)]
pub(crate) struct WorkspaceSemantics {
    graph: NormalizedGraph,
    syntax_provider: ProviderId,
}

impl WorkspaceSemantics {
    /// Builds syntax and binding publications and one normalized graph over one file set.
    ///
    /// The binding provider publishes beside syntax when `binding` enables it and at
    /// least one document carries binding facts. A binding failure - an exhausted
    /// limit, a refused publication - never fails the build: the revision publishes
    /// with the syntax publication alone and the failure lands in the server log.
    pub(crate) fn build<'a>(
        documents: impl IntoIterator<Item = &'a SyntaxDocument>,
        revision: u64,
        previous: Option<&NormalizedGraph>,
        binding: &BindingPolicy,
    ) -> Result<Self, WorkspaceSemanticError> {
        let index_revision = IndexRevision::new(revision)?;
        let source_revision = SourceRevision::new(revision)?;
        let tree_revision = TreeRevision::new(revision)?;
        let provider_revision = ProviderRevision::new(revision)?;
        let limits = PublicationLimits::default();
        let documents: Vec<&SyntaxDocument> = documents.into_iter().collect();
        let mut builder = SyntaxPublicationBuilder::new(
            provider_revision,
            source_revision,
            tree_revision,
            limits,
        )?;
        for document in &documents {
            builder.add_document(document)?;
        }
        let publication = builder.build()?;
        let publications = PublicationSet::empty(limits).replaced(publication)?;
        let publications = Arc::new(with_binding(
            publications,
            &documents,
            binding,
            (provider_revision, source_revision, tree_revision),
            limits,
        ));
        let graph = Normalizer::normalize(
            index_revision,
            source_revision,
            tree_revision,
            &publications,
            previous,
        )?;
        Ok(Self {
            graph,
            syntax_provider: ProviderId::new(SYNTAX_PROVIDER_ID)
                .map_err(SyntaxPublicationError::Identity)?,
        })
    }

    /// Returns captured normalized graph.
    pub(crate) const fn graph(&self) -> &NormalizedGraph {
        &self.graph
    }

    /// Assembles readable symbol for syntax provider-local identity.
    pub(crate) fn assembled(&self, provider_symbol: &str) -> Option<AssembledSymbol> {
        let reference = ContributionReference::new(
            self.syntax_provider.clone(),
            ProviderSymbolId::new(provider_symbol).ok()?,
        );
        let record = self.graph.record_for(&reference)?;
        SymbolAssembler::assemble(
            &self.graph,
            record,
            std::slice::from_ref(&self.syntax_provider),
        )
    }
}

/// Adds the binding provider's publication beside syntax, keeping syntax alone on failure.
///
/// The failure is recorded as a `tracing` warning naming the cause, so the operator
/// can raise the breached `[providers.binding]` bound; the returned set is the one
/// the caller handed in.
fn with_binding(
    publications: PublicationSet,
    documents: &[&SyntaxDocument],
    binding: &BindingPolicy,
    revisions: (ProviderRevision, SourceRevision, TreeRevision),
    publication_limits: PublicationLimits,
) -> PublicationSet {
    if !binding.is_enabled() {
        return publications;
    }
    match binding_replaced(
        &publications,
        documents,
        binding.limits(),
        revisions,
        publication_limits,
    ) {
        Ok(Some(replaced)) => replaced,
        Ok(None) => publications,
        Err(error) => {
            tracing::warn!(
                component = "index",
                operation = "binding.publish",
                error = %error,
                "name binding publication failed; the revision serves the syntax publication alone"
            );
            publications
        }
    }
}

/// Assembles, links, resolves, and publishes name binding facts over `documents`.
///
/// Answers `None` when no document carries binding facts, so an all-text workspace
/// publishes no empty binding publication.
fn binding_replaced(
    publications: &PublicationSet,
    documents: &[&SyntaxDocument],
    limits: &BindingLimits,
    revisions: (ProviderRevision, SourceRevision, TreeRevision),
    publication_limits: PublicationLimits,
) -> Result<Option<PublicationSet>, WorkspaceSemanticError> {
    let units = binding_units(documents)?;
    if units.is_empty() {
        return Ok(None);
    }
    let graph = assemble(&units, limits)?;
    let linked = LinkedGraph::link(&graph, limits)?;
    let resolutions = resolve_all(&linked, limits, &NeverCancelled)?;
    let (provider_revision, source_revision, tree_revision) = revisions;
    let publisher = BindingPublisher::new(
        provider_revision,
        source_revision,
        tree_revision,
        *limits,
        publication_limits,
    )?;
    let publication = publisher.publish(&graph, &linked, &resolutions)?;
    Ok(Some(publications.replaced(publication.into_publication())?))
}

/// Every document's binding facts under its project source-unit identity.
fn binding_units<'documents>(
    documents: &[&'documents SyntaxDocument],
) -> Result<
    Vec<(
        SourceUnitId,
        ContributionOrigin,
        &'documents UnitBindingFacts,
    )>,
    WorkspaceSemanticError,
> {
    let mut units = Vec::new();
    for document in documents {
        let Some(facts) = document.binding() else {
            continue;
        };
        let origin = ContributionOrigin::new(
            Some(SourceLocation::Project { package: None }),
            SourceKind::Authored,
        )?;
        units.push((source_unit(document)?, origin, facts));
    }
    Ok(units)
}

/// Semantic publication failure inside workspace index build.
#[derive(Debug)]
pub(crate) enum WorkspaceSemanticError {
    Revision(RevisionError),
    Syntax(SyntaxPublicationError),
    Publication(PublicationError),
    Normalization(ContributionError),
    Binding(BindingError),
}

impl fmt::Display for WorkspaceSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revision(error) => error.fmt(formatter),
            Self::Syntax(error) => error.fmt(formatter),
            Self::Publication(error) => error.fmt(formatter),
            Self::Normalization(error) => error.fmt(formatter),
            Self::Binding(error) => error.fmt(formatter),
        }
    }
}

impl StdError for WorkspaceSemanticError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Revision(error) => Some(error),
            Self::Syntax(error) => Some(error),
            Self::Publication(error) => Some(error),
            Self::Normalization(error) => Some(error),
            Self::Binding(error) => Some(error),
        }
    }
}

impl From<RevisionError> for WorkspaceSemanticError {
    fn from(error: RevisionError) -> Self {
        Self::Revision(error)
    }
}

impl From<SyntaxPublicationError> for WorkspaceSemanticError {
    fn from(error: SyntaxPublicationError) -> Self {
        Self::Syntax(error)
    }
}

impl From<PublicationError> for WorkspaceSemanticError {
    fn from(error: PublicationError) -> Self {
        Self::Publication(error)
    }
}

impl From<ContributionError> for WorkspaceSemanticError {
    fn from(error: ContributionError) -> Self {
        Self::Normalization(error)
    }
}

impl From<BindingError> for WorkspaceSemanticError {
    fn from(error: BindingError) -> Self {
        Self::Binding(error)
    }
}

impl From<SourceUnitIdError> for WorkspaceSemanticError {
    fn from(error: SourceUnitIdError) -> Self {
        Self::Syntax(SyntaxPublicationError::from(error))
    }
}

#[cfg(test)]
mod tests {
    use rift_core::ProjectPath;
    use rift_syntax::{SyntaxSource, registry};

    use super::{BindingPolicy, WorkspaceSemanticError, WorkspaceSemantics};

    fn document() -> rift_syntax::SyntaxDocument {
        let path = ProjectPath::new("src/lib.rs").expect("path");
        registry::provider_for_extension("rs")
            .expect("rust provider")
            .analyze(SyntaxSource {
                path: &path,
                text: "pub fn beacon() {}\n",
            })
            .expect("document")
    }

    /// Binding is off here, so the record count proves the syntax publication alone.
    #[test]
    fn syntax_graph_assembles_existing_symbol_identity() {
        let document = document();
        let policy = BindingPolicy::new(false, rift_binding::BindingLimits::default());
        let semantics =
            WorkspaceSemantics::build([&document], 7, None, &policy).expect("semantics");
        let identity = "rift://symbol/rust/src/lib.rs/beacon";
        let assembled = semantics.assembled(identity).expect("assembled symbol");
        assert_eq!(
            assembled.identity().map(rift_core::SymbolId::as_str),
            Some(identity)
        );
        assert_eq!(assembled.index_revision().get(), 7);
        assert_eq!(semantics.graph().records().len(), 1);
    }

    #[test]
    fn zero_revision_is_typed_failure() {
        let error =
            WorkspaceSemantics::build(std::iter::empty(), 0, None, &BindingPolicy::default())
                .expect_err("zero revision");
        assert!(matches!(error, WorkspaceSemanticError::Revision(_)));
        assert!(std::error::Error::source(&error).is_some());
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn test_binding_policy_default_matches_the_configuration_defaults() {
        use rift_protocol::configuration::BindingConfiguration;

        assert_eq!(
            super::BindingPolicy::from(&BindingConfiguration::default()),
            BindingPolicy::default(),
            "the [providers.binding] defaults and the rift-binding defaults must not drift"
        );
    }

    /// Pins each `[providers.binding]` default literal to its `rift-binding` constant,
    /// naming the key that drifted; the protocol crate sits below `rift-binding` and
    /// restates the literals.
    #[test]
    fn test_binding_configuration_default_literals_equal_the_binding_constants() {
        let configuration = rift_protocol::configuration::BindingConfiguration::default();
        let cases = [
            (
                "max_unit_scopes",
                configuration.max_unit_scopes,
                rift_binding::UNIT_SCOPES_MAX_DEFAULT,
            ),
            (
                "max_unit_definitions",
                configuration.max_unit_definitions,
                rift_binding::UNIT_DEFINITIONS_MAX_DEFAULT,
            ),
            (
                "max_unit_references",
                configuration.max_unit_references,
                rift_binding::UNIT_REFERENCES_MAX_DEFAULT,
            ),
            (
                "max_unit_links",
                configuration.max_unit_links,
                rift_binding::UNIT_LINKS_MAX_DEFAULT,
            ),
            (
                "max_graph_nodes",
                configuration.max_graph_nodes,
                rift_binding::GRAPH_NODES_MAX_DEFAULT,
            ),
            (
                "max_graph_links",
                configuration.max_graph_links,
                rift_binding::GRAPH_LINKS_MAX_DEFAULT,
            ),
            (
                "max_reference_work",
                configuration.max_reference_work,
                rift_binding::REFERENCE_WORK_MAX_DEFAULT,
            ),
            (
                "max_path_depth",
                configuration.max_path_depth,
                rift_binding::PATH_DEPTH_MAX_DEFAULT,
            ),
            (
                "max_reference_targets",
                configuration.max_reference_targets,
                rift_binding::REFERENCE_TARGETS_MAX_DEFAULT,
            ),
            (
                "max_publication_work",
                configuration.max_publication_work,
                rift_binding::PUBLICATION_WORK_MAX_DEFAULT,
            ),
        ];
        for (key, advertised, enforced) in cases {
            let enforced = u64::try_from(enforced).expect("binding defaults fit u64");
            assert_eq!(
                advertised, enforced,
                "providers.binding.{key} default drifted"
            );
        }
    }

    /// The targets ceiling equals the reference facts one Contribution carries, so an
    /// accepted configuration can never ask the publisher for more targets than it accepts.
    #[test]
    fn test_binding_targets_ceiling_equals_the_contribution_facts_bound() {
        let facts_max =
            u64::try_from(rift_core::CONTRIBUTION_FACTS_MAX).expect("facts bound fits u64");
        assert_eq!(
            rift_protocol::configuration::BINDING_REFERENCE_TARGETS_MAX,
            facts_max
        );
    }

    #[test]
    fn test_binding_policy_from_zero_key_keeps_the_conversion_total() {
        let configuration = rift_protocol::configuration::BindingConfiguration {
            max_unit_scopes: 0,
            enabled: false,
            ..Default::default()
        };
        let policy = super::BindingPolicy::from(&configuration);
        assert!(!policy.is_enabled());
        assert_eq!(
            policy.limits().unit_scopes_max(),
            1,
            "a zero acceptance already refuses converts to the smallest legal bound"
        );
    }

    #[test]
    fn test_build_without_binding_facts_publishes_no_binding_publication() {
        let semantics =
            WorkspaceSemantics::build(std::iter::empty(), 3, None, &BindingPolicy::default())
                .expect("an empty document set publishes");
        let binding = rift_core::ProviderId::new(rift_binding::BINDING_PROVIDER_ID)
            .expect("provider identity");
        assert!(
            semantics
                .graph()
                .publications()
                .provider(&binding)
                .is_none(),
            "no document carries binding facts, so no binding publication exists"
        );
    }

    #[test]
    fn test_binding_error_variant_displays_and_names_its_source() {
        let binding_error = rift_binding::BindingLimits::builder()
            .unit_scopes_max(0)
            .build()
            .expect_err("a zero bound is refused");
        let error = WorkspaceSemanticError::from(binding_error);
        assert!(matches!(error, WorkspaceSemanticError::Binding(_)));
        assert!(!error.to_string().is_empty());
        assert!(std::error::Error::source(&error).is_some());
    }

    #[test]
    fn test_source_unit_error_converts_to_the_syntax_variant() {
        let unit_error = rift_core::SourceUnitId::parse("not-a-source-unit")
            .expect_err("a malformed unit identity is refused");
        let error = WorkspaceSemanticError::from(unit_error);
        assert!(matches!(error, WorkspaceSemanticError::Syntax(_)));
        assert!(!error.to_string().is_empty());
        assert!(std::error::Error::source(&error).is_some());
    }
}
