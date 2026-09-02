use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use rift_binding::{
    BindingError, BindingLimits, BindingPublisher, LinkedGraph, ModuleLayout, NeverCancelled,
    UnitBindingFacts, assemble, resolve_all,
};
use rift_core::{
    ContributionError, ContributionOrigin, ContributionReference, IndexRevision, ProviderId,
    ProviderRevision, ProviderSymbolId, RevisionError, SourceRevision, SourceUnitId,
    SourceUnitIdError, TreeRevision,
};
use rift_protocol::configuration::BindingConfiguration;
use rift_protocol::read::Language;
use rift_provider::{
    AssembledSymbol, CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT, CONTRIBUTIONS_TOTAL_MAX_DEFAULT,
    NormalizedGraph, Normalizer, PROVIDERS_MAX_DEFAULT, PublicationError, PublicationLimits,
    PublicationSet, SymbolAssembler,
};
use rift_syntax::{
    DocumentPlacement, SYNTAX_PROVIDER_ID, SyntaxDocument, SyntaxPublicationBuilder,
    SyntaxPublicationError, registry,
};

use crate::relationship::RelationshipStore;

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

    /// The policy under which no binding provider runs, as a dependency package builds.
    #[must_use]
    pub fn disabled() -> Self {
        Self::new(false, BindingLimits::default())
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

/// One syntax document and the placement its declarations are filed under.
#[derive(Debug)]
pub(crate) struct PlacedDocument<'a> {
    /// The parsed document.
    pub(crate) document: &'a SyntaxDocument,
    /// The unit, origin, and identity path its declarations carry.
    pub(crate) placement: DocumentPlacement,
}

/// Contribution graph captured by one workspace index publication.
#[derive(Debug)]
pub(crate) struct WorkspaceSemantics {
    graph: NormalizedGraph,
    relationships: RelationshipStore,
    syntax_provider: ProviderId,
}

impl WorkspaceSemantics {
    /// Builds syntax and binding publications and one normalized graph over one file set.
    ///
    /// Every document takes the project placement, [`DocumentPlacement::project`];
    /// the build itself is [`Self::build_placed`]. `project_paths` is every path the
    /// index holds, indexed and text files alike: each language provider derives its
    /// module layout from that set, so manifest files such as `Cargo.toml` reach the
    /// layout.
    pub(crate) fn build<'a>(
        documents: impl IntoIterator<Item = &'a SyntaxDocument>,
        project_paths: &[&str],
        revision: u64,
        previous: Option<&NormalizedGraph>,
        binding: &BindingPolicy,
    ) -> Result<Self, WorkspaceSemanticError> {
        let placed = documents
            .into_iter()
            .map(|document| {
                Ok(PlacedDocument {
                    document,
                    placement: DocumentPlacement::project(document)?,
                })
            })
            .collect::<Result<Vec<_>, SyntaxPublicationError>>()?;
        Self::build_placed(&placed, project_paths, revision, previous, binding)
    }

    /// Builds the publications and one normalized graph over documents the caller placed.
    ///
    /// The binding provider publishes beside syntax when `binding` enables it and at
    /// least one document carries binding facts; a dependency package builds under
    /// [`BindingPolicy::disabled`] with an empty `project_paths`. A binding failure -
    /// an exhausted limit, a refused publication, a refused refinement - never fails
    /// the build: the revision publishes with the syntax publication alone and the
    /// failure lands in the server log.
    pub(crate) fn build_placed(
        documents: &[PlacedDocument<'_>],
        project_paths: &[&str],
        revision: u64,
        previous: Option<&NormalizedGraph>,
        binding: &BindingPolicy,
    ) -> Result<Self, WorkspaceSemanticError> {
        let index_revision = IndexRevision::new(revision)?;
        let source_revision = SourceRevision::new(revision)?;
        let tree_revision = TreeRevision::new(revision)?;
        let provider_revision = ProviderRevision::new(revision)?;
        let limits = publication_limits(binding)?;
        let mut builder = SyntaxPublicationBuilder::new(
            provider_revision,
            source_revision,
            tree_revision,
            limits,
        )?;
        for placed in documents {
            builder.add_document_placed(placed.document, &placed.placement)?;
        }
        let publication = builder.build()?;
        let publications = PublicationSet::empty(limits).replaced(publication)?;
        let publications = Arc::new(with_binding(
            publications,
            documents,
            project_paths,
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
        let relationships = RelationshipStore::build(&graph);
        Ok(Self {
            graph,
            relationships,
            syntax_provider: ProviderId::new(SYNTAX_PROVIDER_ID)
                .map_err(SyntaxPublicationError::Identity)?,
        })
    }

    /// Returns captured normalized graph.
    pub(crate) const fn graph(&self) -> &NormalizedGraph {
        &self.graph
    }

    /// Returns the symbol reference adjacency built from this revision's graph.
    pub(crate) const fn relationships(&self) -> &RelationshipStore {
        &self.relationships
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

/// Publication bounds sized so the configured binding graph fits beside syntax.
///
/// The binding provider publishes one Contribution per graph node, and the
/// graph is already bounded by `[providers.binding] max_graph_nodes`; the
/// per-provider bound follows that configured bound so acceptance cannot
/// refuse work the graph bounds admit. The total keeps one default of
/// headroom for the syntax publication.
fn publication_limits(
    binding: &BindingPolicy,
) -> Result<PublicationLimits, WorkspaceSemanticError> {
    let per_provider =
        CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT.max(binding.limits().graph_nodes_max());
    let total = CONTRIBUTIONS_TOTAL_MAX_DEFAULT
        .max(per_provider.saturating_add(CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT));
    Ok(PublicationLimits::new(
        PROVIDERS_MAX_DEFAULT,
        per_provider,
        total,
    )?)
}

/// Adds the binding provider's publication beside syntax, keeping syntax alone on failure.
///
/// The failure is recorded as a `tracing` warning naming the cause. Publication
/// derives its per-provider bound from `[providers.binding] max_graph_nodes`
/// (see `publication_limits`), so raising that key also raises the bound that
/// would otherwise refuse the graph it admits; the returned set is the one the
/// caller handed in.
fn with_binding(
    publications: PublicationSet,
    documents: &[PlacedDocument<'_>],
    project_paths: &[&str],
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
        project_paths,
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
    documents: &[PlacedDocument<'_>],
    project_paths: &[&str],
    limits: &BindingLimits,
    revisions: (ProviderRevision, SourceRevision, TreeRevision),
    publication_limits: PublicationLimits,
) -> Result<Option<PublicationSet>, WorkspaceSemanticError> {
    let units = binding_units(documents, project_paths, limits)?;
    if units.is_empty() {
        return Ok(None);
    }
    let assembled: Vec<(SourceUnitId, ContributionOrigin, &UnitBindingFacts)> = units
        .iter()
        .map(|(unit, origin, facts)| (unit.clone(), origin.clone(), facts))
        .collect();
    let graph = assemble(&assembled, limits)?;
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

/// Per-language module layouts, each resolved once from the provider registry.
///
/// The cache holds one entry per distinct document language, so a provider's
/// `binding_layout` runs once per provider, never per document.
struct ModuleLayouts<'paths> {
    project_paths: &'paths [&'paths str],
    layouts: Vec<(Language, Option<Box<dyn ModuleLayout + Send + Sync>>)>,
}

impl<'paths> ModuleLayouts<'paths> {
    const fn new(project_paths: &'paths [&'paths str]) -> Self {
        Self {
            project_paths,
            layouts: Vec::new(),
        }
    }

    /// The layout for `language`, resolved through the registry on first use.
    ///
    /// The scan runs over one entry per language already seen, a set the shipped
    /// provider registry bounds.
    fn layout_for(&mut self, language: &Language) -> Option<&(dyn ModuleLayout + Send + Sync)> {
        let held = self.layouts.iter().position(|(seen, _)| seen == language);
        let position = held.unwrap_or_else(|| {
            let layout = registry::provider_for_language(language)
                .and_then(|provider| provider.binding_layout(self.project_paths));
            self.layouts.push((language.clone(), layout));
            self.layouts.len() - 1
        });
        self.layouts[position].1.as_deref()
    }
}

/// The unit's facts under `layout`; no layout keeps the extraction-time candidates.
fn refined_unit_facts(
    layout: Option<&(dyn ModuleLayout + Send + Sync)>,
    unit_path: &str,
    facts: &UnitBindingFacts,
    limits: &BindingLimits,
) -> Result<UnitBindingFacts, BindingError> {
    match layout {
        Some(layout) => layout.refined_facts(unit_path, facts, limits),
        None => Ok(facts.clone()),
    }
}

/// Every document's binding facts, refined under its language's module layout.
fn binding_units(
    documents: &[PlacedDocument<'_>],
    project_paths: &[&str],
    limits: &BindingLimits,
) -> Result<Vec<(SourceUnitId, ContributionOrigin, UnitBindingFacts)>, WorkspaceSemanticError> {
    let mut layouts = ModuleLayouts::new(project_paths);
    let mut units = Vec::new();
    for placed in documents {
        let Some(facts) = placed.document.binding() else {
            continue;
        };
        let layout = layouts.layout_for(placed.document.language());
        let facts = refined_unit_facts(layout, placed.document.path().as_str(), facts, limits)?;
        units.push((
            placed.placement.unit().clone(),
            placed.placement.origin().clone(),
            facts,
        ));
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
        let semantics = WorkspaceSemantics::build([&document], &["src/lib.rs"], 7, None, &policy)
            .expect("semantics");
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
            WorkspaceSemantics::build(std::iter::empty(), &[], 0, None, &BindingPolicy::default())
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
            WorkspaceSemantics::build(std::iter::empty(), &[], 3, None, &BindingPolicy::default())
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

    /// A document language without a provider layout keeps its extraction-time candidates.
    #[test]
    fn test_refined_unit_facts_without_layout_keeps_extraction_candidates() {
        use rift_binding::{
            BindingLimits, DefinitionOrder, Name, ScopeKind, UnitBindingFacts, UnitDefinition,
            UnitModuleDeclaration, VisibilitySpelling,
        };
        use rift_core::{ExactKind, SourceRange};

        let limits = BindingLimits::default();
        let mut builder = UnitBindingFacts::builder(limits);
        let range = SourceRange::new(0, 8).expect("fixture range");
        let root = builder
            .scope(ScopeKind::Module, range, None)
            .expect("root scope accepted");
        let name = Name::new("x").expect("fixture name");
        let definition = UnitDefinition::new(
            root,
            name,
            range,
            ExactKind("stub.module".to_owned()),
            DefinitionOrder::Item,
            VisibilitySpelling::Private,
        );
        let definition = builder.definition(definition).expect("definition accepted");
        let declaration = UnitModuleDeclaration::new(definition, vec!["src/x.rs".to_owned()]);
        builder
            .module_declaration(declaration)
            .expect("declaration accepted");
        let facts = builder.build();
        let kept = super::refined_unit_facts(None, "src/lib.rs", &facts, &limits)
            .expect("no layout keeps the facts");
        assert_eq!(
            kept, facts,
            "an absent layout keeps extraction-time candidates"
        );
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

    /// Default binding limits exceed the publication default, so the graph bound wins.
    #[test]
    fn test_publication_limits_default_policy_follows_the_graph_node_default() {
        let policy = BindingPolicy::default();
        let derived = super::publication_limits(&policy).expect("derived limits");
        assert_eq!(
            derived.contributions_per_provider_max(),
            rift_binding::GRAPH_NODES_MAX_DEFAULT,
            "the default binding graph bound exceeds the publication default"
        );
        assert_eq!(
            derived.contributions_total_max(),
            rift_binding::GRAPH_NODES_MAX_DEFAULT
                + rift_provider::CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT
        );
    }

    /// A graph bound below the publication default keeps the publication floor.
    #[test]
    fn test_publication_limits_small_graph_bound_keeps_the_publication_floor() {
        let limits = rift_binding::BindingLimits::builder()
            .graph_nodes_max(10)
            .build()
            .expect("small graph bound accepted");
        let policy = BindingPolicy::new(true, limits);
        let derived = super::publication_limits(&policy).expect("derived limits");
        assert_eq!(
            derived.contributions_per_provider_max(),
            rift_provider::CONTRIBUTIONS_PER_PROVIDER_MAX_DEFAULT,
            "a graph bound below the publication default keeps the publication floor"
        );
        assert_eq!(
            derived.contributions_total_max(),
            rift_provider::CONTRIBUTIONS_TOTAL_MAX_DEFAULT,
            "the summed bound also stays below the total floor"
        );
    }

    /// Acceptance can never refuse work the configured graph bound admits.
    #[test]
    fn test_publication_limits_per_provider_bound_never_undercuts_the_graph_bound() {
        let cases = [
            rift_binding::BindingLimits::default(),
            rift_binding::BindingLimits::builder()
                .graph_nodes_max(10)
                .build()
                .expect("small graph bound accepted"),
        ];
        for limits in cases {
            let policy = BindingPolicy::new(true, limits);
            let derived = super::publication_limits(&policy).expect("derived limits");
            assert!(
                derived.contributions_per_provider_max() >= policy.limits().graph_nodes_max(),
                "publication acceptance must not refuse work the graph bounds admit"
            );
        }
    }
}
