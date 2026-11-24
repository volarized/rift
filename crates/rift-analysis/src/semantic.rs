use std::sync::Arc;

mod captured;
pub use captured::PreparedProjectSemantics;

use rift_core::{
    ContributionReference, IndexRevision, ProjectPath, ProviderId, ProviderRevision,
    ProviderSymbolId, SourceRevision, TreeRevision,
};
use rift_error::{ErrorContext, ErrorValue, RiftError};
use rift_provider::{
    AssembledSymbol, NormalizedGraph, Normalizer, PROVIDERS_MAX_DEFAULT, PublicationLimits,
    PublicationSet, SymbolAssembler,
};
use rift_syntax::{
    DocumentPlacement, SYNTAX_PROVIDER_ID, SyntaxDocument, SyntaxFacts, SyntaxPublicationBuilder,
};

use crate::edge::EdgeStore;
use crate::static_resolution::{StaticOccurrence, StaticResolution, StaticSource};

/// Publication bounds for a build that holds at most `declarations_max` declarations.
///
/// The workspace index publishes one provider, so the set bound is the per-provider bound.
/// The caller states the capacity it owns: a workspace build passes the `[source]` table's
/// `declarations`, a package analysis its accepted `ExactPackageLimits` bound.
///
/// # Errors
///
/// Returns [`RiftError`] when `declarations_max` is zero.
fn publication_limits(declarations_max: usize) -> Result<PublicationLimits, RiftError> {
    PublicationLimits::new(PROVIDERS_MAX_DEFAULT, declarations_max, declarations_max)
}

/// One syntax document and the placement its declarations are filed under.
#[derive(Debug)]
pub struct PlacedDocument<'a> {
    /// The parsed document.
    pub document: &'a SyntaxDocument,
    /// The unit, origin, and identity path its declarations carry.
    pub placement: DocumentPlacement,
}

/// Path-independent syntax facts and the placement their declarations are filed under.
#[derive(Debug)]
pub struct PlacedFacts<'a> {
    /// Compact syntax facts for one parsed source file.
    pub facts: &'a SyntaxFacts,
    /// Project-relative source path used for error and bound reporting.
    pub path: &'a ProjectPath,
    /// The unit, origin, and identity path the declarations carry.
    pub placement: DocumentPlacement,
}

/// Contribution graph captured by one workspace index publication.
#[derive(Debug)]
pub struct WorkspaceSemantics {
    graph: NormalizedGraph,
    edges: EdgeStore,
    syntax_provider: ProviderId,
    static_resolution: StaticResolution,
    static_occurrence_order: Vec<usize>,
    outside_namespaces: std::collections::BTreeSet<rift_core::SourceUnitId>,
    captured_sources: captured::AdmittedSources,
}

/// One build's captured semantics and documents not included in publication.
#[derive(Debug)]
pub struct BuiltSemantics {
    /// The graph built over the documents that fit.
    pub semantics: WorkspaceSemantics,
    /// The documents the publication had no room for, in the order they were offered.
    pub beyond_declaration_bound: Vec<ProjectPath>,
    /// Documents whose Contribution the publication refused.
    pub refused_contributions: Vec<(ProjectPath, RiftError)>,
}

/// An admitted syntax publication awaiting graph construction.
#[derive(Debug)]
#[must_use]
pub struct PreparedSemantics {
    publications: Arc<PublicationSet>,
    modules: std::collections::BTreeMap<rift_core::SourceUnitId, rift_core::SymbolId>,
    revision: u64,
    /// Documents outside the publication's declaration capacity.
    pub beyond_declaration_bound: Vec<ProjectPath>,
    /// Documents whose contributions were refused.
    pub refused_contributions: Vec<(ProjectPath, RiftError)>,
}

impl PreparedSemantics {
    /// Builds the graph after the caller has handled omitted documents.
    ///
    /// # Errors
    /// Returns a typed error when graph validation fails.
    pub fn build(
        self,
        edges_max: usize,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        self.build_with_external(edges_max, previous, None, None)
    }

    fn build_with_external(
        self,
        edges_max: usize,
        previous: Option<&NormalizedGraph>,
        external: Option<&ExternalTarget<'_>>,
        static_inputs: Option<StaticInputs<'_, '_>>,
    ) -> Result<BuiltSemantics, RiftError> {
        let index_revision = IndexRevision::new(self.revision)?;
        let source_revision = SourceRevision::new(self.revision)?;
        let tree_revision = TreeRevision::new(self.revision)?;
        let graph = match external {
            Some(external) => Normalizer::normalize_with(
                index_revision,
                source_revision,
                tree_revision,
                &self.publications,
                external,
            )?,
            None => Normalizer::normalize(
                index_revision,
                source_revision,
                tree_revision,
                &self.publications,
                previous,
            )?,
        };
        let syntax_provider = ProviderId::new(SYNTAX_PROVIDER_ID)?;
        let (edges, static_resolution) = match static_inputs {
            Some((sources, input))
                if self.beyond_declaration_bound.is_empty()
                    && self.refused_contributions.is_empty() =>
            {
                EdgeStore::build_resolving(&graph, self.modules, edges_max, || {
                    crate::static_resolution::resolve(
                        &graph,
                        sources,
                        input,
                        edges_max.saturating_mul(16),
                    )
                })
            }
            _ => (
                EdgeStore::build_placed(&graph, self.modules, edges_max),
                StaticResolution::default(),
            ),
        };
        let mut semantics = WorkspaceSemantics {
            graph,
            edges,
            syntax_provider,
            static_resolution: StaticResolution::default(),
            static_occurrence_order: Vec::new(),
            outside_namespaces: std::collections::BTreeSet::new(),
            captured_sources: captured::AdmittedSources::default(),
        };
        semantics.hold_static_resolution(static_resolution);
        Ok(BuiltSemantics {
            semantics,
            beyond_declaration_bound: self.beyond_declaration_bound,
            refused_contributions: self.refused_contributions,
        })
    }
}

type ExternalTarget<'input> =
    dyn Fn(&rift_core::ContributionReference) -> Option<rift_core::SymbolId> + 'input;
type StaticInputs<'call, 'source> = (
    &'call [StaticSource<'source>],
    &'call crate::NamespaceInput<'source>,
);

impl StaticResolution {
    fn retained_size<T: get_size2::GetSizeTracker>(&self, mut tracker: T) -> (usize, T) {
        let mut bytes = self
            .external
            .capacity()
            .saturating_mul(std::mem::size_of::<
                crate::static_resolution::ExternalOccurrence,
            >())
            .saturating_add(
                self.external
                    .iter()
                    .map(|occurrence| occurrence.target.as_str().len())
                    .fold(0, usize::saturating_add),
            );
        bytes = bytes.saturating_add(
            self.occurrences
                .capacity()
                .saturating_mul(std::mem::size_of::<StaticOccurrence>()),
        );
        for occurrence in &self.occurrences {
            let (binding, next) =
                get_size2::GetSize::get_heap_size_with_tracker(&occurrence.binding, tracker);
            let (target_binding, next) =
                get_size2::GetSize::get_heap_size_with_tracker(&occurrence.target_binding, next);
            let (target, next) = match &occurrence.target {
                rift_provider::NormalizedTarget::Symbol(symbol) => {
                    get_size2::GetSize::get_heap_size_with_tracker(symbol, next)
                }
                rift_provider::NormalizedTarget::Contribution(reference) => {
                    get_size2::GetSize::get_heap_size_with_tracker(reference, next)
                }
            };
            bytes = bytes
                .saturating_add(binding)
                .saturating_add(target_binding)
                .saturating_add(target);
            let (from, next) =
                get_size2::GetSize::get_heap_size_with_tracker(&occurrence.edge_from, next);
            bytes = bytes.saturating_add(from);
            tracker = next;
        }
        let (coverage, tracker) =
            rift_protocol::retained_map_material(&self.edge_calls, tracker, |coverage, tracker| {
                let (language, tracker) =
                    get_size2::GetSize::get_heap_size_with_tracker(&coverage.language, tracker);
                (
                    language.saturating_add(
                        coverage
                            .unresolved
                            .capacity()
                            .saturating_mul(std::mem::size_of::<rift_syntax::ByteRange>()),
                    ),
                    tracker,
                )
            });
        (bytes.saturating_add(coverage), tracker)
    }
}

impl WorkspaceSemantics {
    /// Whether static capture establishes every call edge in the selected source range.
    ///
    /// Incoming reads require every captured source of the same language, across its dialects.
    /// JavaScript and TypeScript share incoming coverage because their imports can cross languages.
    /// Parse errors, unresolved calls, missing identities, and exhausted bounds prevent completeness.
    #[must_use]
    pub fn static_edge_calls_complete(
        &self,
        unit: &rift_core::SourceUnitId,
        range: Option<rift_syntax::ByteRange>,
    ) -> bool {
        if self.static_resolution.exhausted || !self.edges.is_complete() {
            return false;
        }
        let coverage = &self.static_resolution.edge_calls;
        let Some(selected) = coverage.get(unit) else {
            return false;
        };
        match range {
            Some(range) => {
                !selected.unavailable
                    && !selected
                        .unresolved
                        .iter()
                        .any(|site| range.encloses(*site) || site.encloses(range))
            }
            None => coverage
                .values()
                .filter(|source| match selected.language.name.as_str() {
                    "javascript" | "typescript" => {
                        matches!(source.language.name.as_str(), "javascript" | "typescript")
                    }
                    _ => source.language.name == selected.language.name,
                })
                .all(|source| !source.unavailable && source.unresolved.is_empty()),
        }
    }
    /// Builds one syntax publication and one normalized graph over one file set.
    ///
    /// Every document takes the project placement, [`DocumentPlacement::project`];
    /// the build itself is [`Self::build_placed`].
    ///
    /// # Errors
    ///
    /// Returns a typed error when publication or graph validation fails.
    pub fn build<'a>(
        documents: impl IntoIterator<Item = &'a SyntaxDocument>,
        declarations_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        let placed = documents
            .into_iter()
            .map(|document| {
                Ok(PlacedDocument {
                    document,
                    placement: DocumentPlacement::project(document)?,
                })
            })
            .collect::<Result<Vec<_>, RiftError>>()?;
        Self::build_placed(&placed, declarations_max, revision, previous)
    }

    /// Builds the publication and one normalized graph over documents the caller placed.
    ///
    /// The publication holds `declarations_max` declarations. A document that would cross
    /// that bound stops the pass: it and every document after it are named in
    /// [`BuiltSemantics::beyond_declaration_bound`], so the index leaves them out and
    /// serves what fits instead of refusing the whole build. Stopping at the first document
    /// that does not fit is the rule the file bound already follows, so which documents
    /// survive does not depend on how the walk happened to order them.
    ///
    /// A document whose declarations the syntax publication refuses for any other reason
    /// names itself in the error, so the index can leave that one file out instead.
    ///
    /// # Errors
    ///
    /// Returns a typed error when publication or graph validation fails.
    pub fn build_placed(
        documents: &[PlacedDocument<'_>],
        declarations_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        let facts = documents
            .iter()
            .map(|placed| PlacedFacts {
                facts: placed.document.facts(),
                path: placed.document.path(),
                placement: placed.placement.clone(),
            })
            .collect::<Vec<_>>();
        Self::build_facts_placed(&facts, declarations_max, revision, previous)
    }

    /// Builds one project publication from path-independent syntax facts.
    /// Contribution refusals are returned by path so the workspace index can leave all
    /// refused files out in one build pass.
    ///
    /// # Errors
    ///
    /// Returns a typed error when one source path, publication, or graph is invalid.
    pub fn build_project_facts<'a>(
        documents: impl IntoIterator<Item = (&'a SyntaxFacts, &'a ProjectPath)>,
        declarations_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        Self::build_project_facts_with_edges(
            documents,
            declarations_max,
            crate::EDGES_MAX,
            revision,
            previous,
        )
    }

    /// Builds project semantics under caller-selected declaration and edge bounds.
    ///
    /// # Errors
    /// Returns a typed error when source placement, publication, or graph validation fails.
    pub fn build_project_facts_with_edges<'a>(
        documents: impl IntoIterator<Item = (&'a SyntaxFacts, &'a ProjectPath)>,
        declarations_max: usize,
        edges_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        Self::prepare_project_facts(documents, declarations_max, revision)?
            .build(edges_max, previous)
    }

    /// Admits project declarations before graph construction.
    ///
    /// # Errors
    /// Returns a typed error when source placement or publication validation fails.
    pub fn prepare_project_facts<'a>(
        documents: impl IntoIterator<Item = (&'a SyntaxFacts, &'a ProjectPath)>,
        declarations_max: usize,
        revision: u64,
    ) -> Result<PreparedSemantics, RiftError> {
        let placed = documents
            .into_iter()
            .map(|(facts, path)| {
                Ok(PlacedFacts {
                    facts,
                    path,
                    placement: DocumentPlacement::project_path(path)?,
                })
            })
            .collect::<Result<Vec<_>, RiftError>>()?;
        Self::prepare_facts_placed(&placed, declarations_max, revision, true)
    }

    /// Builds one publication and graph over compact syntax facts with explicit source paths.
    ///
    /// # Errors
    ///
    /// Returns a typed error when publication or graph validation fails.
    pub fn build_facts_placed(
        documents: &[PlacedFacts<'_>],
        declarations_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        Self::build_facts_placed_with_edges(
            documents,
            declarations_max,
            crate::EDGES_MAX,
            revision,
            previous,
        )
    }

    /// Builds placed facts under caller-selected declaration and edge bounds.
    ///
    /// # Errors
    /// Returns a typed error when publication or graph validation fails.
    pub fn build_facts_placed_with_edges(
        documents: &[PlacedFacts<'_>],
        declarations_max: usize,
        edges_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        Self::prepare_facts_placed(documents, declarations_max, revision, false)?
            .build(edges_max, previous)
    }

    pub(crate) fn build_facts_placed_with_external(
        documents: &[PlacedFacts<'_>],
        declarations_max: usize,
        edges_max: usize,
        revision: u64,
        external: &ExternalTarget<'_>,
        static_inputs: StaticInputs<'_, '_>,
    ) -> Result<BuiltSemantics, RiftError> {
        Self::prepare_facts_placed(documents, declarations_max, revision, false)?
            .build_with_external(edges_max, None, Some(external), Some(static_inputs))
    }

    fn prepare_facts_placed(
        documents: &[PlacedFacts<'_>],
        declarations_max: usize,
        revision: u64,
        collect_refused_contributions: bool,
    ) -> Result<PreparedSemantics, RiftError> {
        Self::prepare_facts_with(
            documents,
            declarations_max,
            revision,
            collect_refused_contributions,
            |_, placed, builder| builder.add_facts_placed(placed.facts, &placed.placement),
        )
    }

    fn prepare_facts_with(
        documents: &[PlacedFacts<'_>],
        declarations_max: usize,
        revision: u64,
        collect_refused_contributions: bool,
        mut add: impl FnMut(
            usize,
            &PlacedFacts<'_>,
            &mut SyntaxPublicationBuilder,
        ) -> Result<(), RiftError>,
    ) -> Result<PreparedSemantics, RiftError> {
        let source_revision = SourceRevision::new(revision)?;
        let tree_revision = TreeRevision::new(revision)?;
        let provider_revision = ProviderRevision::new(revision)?;
        let limits = publication_limits(declarations_max)?;
        let mut builder = SyntaxPublicationBuilder::new(
            provider_revision,
            source_revision,
            tree_revision,
            limits,
        )?;
        let mut beyond_declaration_bound = Vec::new();
        let mut refused_contributions = Vec::new();
        for (offered, placed) in documents.iter().enumerate() {
            if placed
                .facts
                .symbols()
                .len()
                .saturating_add(placed.placement.aliases_count())
                > builder.declarations_remaining()
            {
                beyond_declaration_bound = documents[offered..]
                    .iter()
                    .map(|left_out| left_out.path.clone())
                    .collect();
                break;
            }
            match add(offered, placed, &mut builder) {
                Ok(()) => {}
                Err(error)
                    if collect_refused_contributions
                        && error.slug().as_str().starts_with("rift.core.contribution_") =>
                {
                    let error = error.with(ErrorContext::new(
                        "path",
                        ErrorValue::path(placed.path.as_str()),
                    ));
                    refused_contributions.push((placed.path.clone(), error));
                }
                Err(error) => {
                    return error
                        .with(ErrorContext::new(
                            "path",
                            ErrorValue::path(placed.path.as_str()),
                        ))
                        .fail();
                }
            }
        }
        let publication = builder.build()?;
        let publications = Arc::new(PublicationSet::empty(limits).replaced(publication)?);
        let modules = documents
            .iter()
            .filter_map(|document| {
                let placement = &document.placement;
                Some((placement.unit().clone(), placement.module()?.clone()))
            })
            .collect();
        Ok(PreparedSemantics {
            publications,
            modules,
            revision,
            beyond_declaration_bound,
            refused_contributions,
        })
    }

    /// Returns captured normalized graph.
    #[must_use]
    pub const fn graph(&self) -> &NormalizedGraph {
        &self.graph
    }

    /// Whether no namespace reaches `unit` by construction, so its declarations carry no
    /// identity because the language assigns none there, not because analysis failed.
    ///
    /// A Rust binary, example, integration test, benchmark, or build script, and the
    /// modules it declares, sits outside every library namespace.
    #[must_use]
    pub fn outside_namespaces(&self, unit: &rift_core::SourceUnitId) -> bool {
        self.outside_namespaces.contains(unit)
    }

    /// Returns the symbol reference adjacency built from this revision's graph.
    #[must_use]
    pub const fn edges(&self) -> &EdgeStore {
        &self.edges
    }

    fn hold_static_resolution(&mut self, mut resolution: StaticResolution) {
        resolution.occurrences.retain(|occurrence| {
            !matches!(
                occurrence.edge_kind,
                Some(
                    rift_protocol::read::EdgeKind::Contains
                        | rift_protocol::read::EdgeKind::Overrides
                )
            )
        });
        // Containment and override occurrences live on as edges alone; the publication
        // keeps the remaining occurrences without the room the others took.
        resolution.occurrences.shrink_to_fit();
        self.static_occurrence_order = (0..resolution.occurrences.len()).collect();
        self.static_occurrence_order.sort_unstable_by_key(|index| {
            (
                canonical_occurrence_target(&resolution.occurrences[*index]),
                *index,
            )
        });
        self.static_resolution = resolution;
    }

    /// Leaves the static pass to a later publication of the same sources: this one holds
    /// no static occurrence and reports unavailable call coverage for every unit.
    pub(crate) fn defer_static_resolution(&mut self, sources: &[StaticSource<'_>]) {
        self.static_resolution = crate::static_resolution::deferred(sources);
        self.static_occurrence_order = Vec::new();
    }

    pub(crate) fn static_occurrences(&self) -> &[StaticOccurrence] {
        &self.static_resolution.occurrences
    }

    /// Resolved occurrences for one canonical target in this captured publication.
    pub fn static_usages(
        &self,
        target: &rift_core::SymbolId,
    ) -> impl Iterator<
        Item = (
            &rift_core::DeclarationBinding,
            rift_syntax::ByteRange,
            rift_core::FileDigest,
            rift_core::ReferenceRole,
        ),
    > {
        let matches = |index: &usize| {
            canonical_occurrence_target(&self.static_resolution.occurrences[*index])
        };
        let start = self
            .static_occurrence_order
            .partition_point(|index| matches(index) < Some(target));
        let end = self
            .static_occurrence_order
            .partition_point(|index| matches(index) <= Some(target));
        self.static_occurrence_order[start..end]
            .iter()
            .map(|index| {
                let occurrence = &self.static_resolution.occurrences[*index];
                (
                    &occurrence.binding,
                    occurrence.callee_range,
                    occurrence.source_digest,
                    occurrence.role,
                )
            })
    }

    /// Sites the static pass left without a target, counted per edge kind. A site counts
    /// when neither a selected source nor a named dependency or standard library
    /// declares what it spells.
    #[must_use]
    pub const fn static_targets_omitted(
        &self,
    ) -> &std::collections::BTreeMap<rift_protocol::read::EdgeKind, u64> {
        &self.static_resolution.omitted
    }

    /// Number of captured sources that could not supply static evidence.
    #[must_use]
    pub const fn static_sources_unavailable(&self) -> usize {
        self.static_resolution.unavailable_sources
    }

    #[cfg(test)]
    pub(crate) fn from_graph(
        graph: NormalizedGraph,
        syntax_provider: ProviderId,
        edges_max: usize,
    ) -> Self {
        let edges = EdgeStore::build_capped(&graph, edges_max);
        Self {
            graph,
            edges,
            syntax_provider,
            static_resolution: StaticResolution::default(),
            static_occurrence_order: Vec::new(),
            outside_namespaces: std::collections::BTreeSet::new(),
            captured_sources: captured::AdmittedSources::default(),
        }
    }

    /// Returns the exact captured syntax Contribution for a provider-local identity.
    #[must_use]
    pub fn syntax_contribution(&self, provider_symbol: &str) -> Option<&rift_core::Contribution> {
        let publication = self.graph.publications().provider(&self.syntax_provider)?;
        let key = rift_core::ContributionKey::new(
            self.syntax_provider.clone(),
            publication.revision(),
            ProviderSymbolId::for_symbol(provider_symbol).ok()?,
        );
        self.graph.contribution(&key)
    }

    /// Assembles readable symbol for syntax provider-local identity.
    #[must_use]
    pub fn assembled(&self, provider_symbol: &str) -> Option<AssembledSymbol> {
        let reference = ContributionReference::new(
            self.syntax_provider.clone(),
            ProviderSymbolId::for_symbol(provider_symbol).ok()?,
        );
        let record = self.graph.record_for(&reference)?;
        SymbolAssembler::assemble(
            &self.graph,
            record,
            std::slice::from_ref(&self.syntax_provider),
        )
    }

    /// Position of the normalized record that holds a syntax provider-local identity.
    ///
    /// Declarations merged into one record share its position, so a pass over every
    /// declaration answers [`Self::readable_identity_at`] once per record.
    #[must_use]
    pub fn record_position(&self, provider_symbol: &str) -> Option<usize> {
        let reference = ContributionReference::new(
            self.syntax_provider.clone(),
            ProviderSymbolId::for_symbol(provider_symbol).ok()?,
        );
        self.graph.record_position_for(&reference)
    }

    /// Established identity of the record at `position` when that record assembles into a
    /// readable symbol.
    ///
    /// Answers what [`Self::assembled`] followed by its portable facts and identity answers,
    /// without assembling the record's references, edges, and presentation.
    #[must_use]
    pub fn readable_identity_at(&self, position: usize) -> Option<&rift_core::SymbolId> {
        let record = self.graph.records().get(position)?;
        if !SymbolAssembler::assembles_facts(&self.graph, record) {
            return None;
        }
        record.identity()
    }

    /// Assembles one captured record, including records with no source declaration.
    #[must_use]
    pub fn assembled_record(&self, record: &rift_core::SymbolRecord) -> Option<AssembledSymbol> {
        SymbolAssembler::assemble(
            &self.graph,
            record,
            std::slice::from_ref(&self.syntax_provider),
        )
    }

    /// The references and edges of the captured graph by the Contribution that supplies
    /// them, for a caller that assembles every record.
    #[must_use]
    pub fn assembly_sources(&self) -> rift_provider::AssemblySources<'_> {
        rift_provider::AssemblySources::new(&self.graph)
    }

    /// Assembles one captured record as [`Self::assembled_record`] does, reading its
    /// references and edges from `sources`.
    #[must_use]
    pub fn assembled_record_from(
        &self,
        sources: &rift_provider::AssemblySources<'_>,
        record: &rift_core::SymbolRecord,
    ) -> Option<AssembledSymbol> {
        SymbolAssembler::assemble_from(
            &self.graph,
            sources,
            record,
            std::slice::from_ref(&self.syntax_provider),
        )
    }
}

impl get_size2::GetSize for WorkspaceSemantics {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(get_size2::GetSize::get_heap_size(self))
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = get_size2::GetSize::get_heap_size_with_tracker(self, tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: get_size2::GetSizeTracker>(
        &self,
        mut tracker: T,
    ) -> (usize, T) {
        let mut bytes: usize = 0;
        let (field, next) = get_size2::GetSize::get_heap_size_with_tracker(&self.graph, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) = get_size2::GetSize::get_heap_size_with_tracker(&self.edges, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) =
            get_size2::GetSize::get_heap_size_with_tracker(&self.syntax_provider, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (field, next) =
            get_size2::GetSize::get_heap_size_with_tracker(&self.outside_namespaces, tracker);
        bytes = bytes.saturating_add(field);
        tracker = next;
        let (captured, tracker) = self.captured_sources.retained_size(tracker);
        bytes = bytes.saturating_add(captured);
        let (static_resolution, tracker) = self.static_resolution.retained_size(tracker);
        let ordering = self
            .static_occurrence_order
            .capacity()
            .saturating_mul(std::mem::size_of::<usize>());
        (
            bytes
                .saturating_add(static_resolution)
                .saturating_add(ordering),
            tracker,
        )
    }
}

fn canonical_occurrence_target(occurrence: &StaticOccurrence) -> Option<&rift_core::SymbolId> {
    match &occurrence.target {
        rift_provider::NormalizedTarget::Symbol(target) => Some(target),
        rift_provider::NormalizedTarget::Contribution(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use rift_core::ProjectPath;
    use rift_syntax::{DocumentPlacement, SyntaxLimits, SyntaxSource, registry};

    use super::{PlacedDocument, WorkspaceSemantics, publication_limits};

    fn document() -> rift_syntax::SyntaxDocument {
        let path = ProjectPath::new("src/lib.rs").expect("path");
        registry::provider_for_extension("rs")
            .expect("rust provider")
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: "pub fn beacon() {}\n",
                },
                SyntaxLimits::default(),
            )
            .expect("document")
    }

    /// One document declaring `count` functions, filed at `path`.
    fn declaring(path: &str, count: usize) -> rift_syntax::SyntaxDocument {
        let path = ProjectPath::new(path.to_owned()).expect("path");
        let mut text = String::new();
        for index in 0..count {
            writeln!(text, "pub fn beacon_{index}() {{}}").expect("a string write succeeds");
        }
        registry::provider_for_extension("rs")
            .expect("rust provider")
            .analyze(
                SyntaxSource {
                    path: &path,
                    text: &text,
                },
                SyntaxLimits::default(),
            )
            .expect("document")
    }

    #[test]
    fn syntax_graph_assembles_existing_symbol_identity() {
        let document = document();
        let identity = rift_protocol::identity::SymbolIdentity::new(
            rift_protocol::identity::SymbolOwner::Local,
            document.language().clone(),
            vec!["fixture".to_owned(), "beacon".to_owned()],
        )
        .expect("canonical fixture identity");
        let identity = rift_core::SymbolId::new(identity.wire_identity()).expect("fixture symbol");
        let placement = DocumentPlacement::project(&document)
            .expect("physical project placement")
            .with_identity_anchors(std::collections::BTreeMap::from([(
                "beacon".to_owned(),
                identity.clone(),
            )]));
        let semantics = WorkspaceSemantics::build_placed(
            &[PlacedDocument {
                document: &document,
                placement,
            }],
            1,
            7,
            None,
        )
        .expect("semantics")
        .semantics;
        let assembled = semantics
            .assembled("rift://symbol/rust/src/lib.rs/beacon")
            .expect("assembled symbol");
        assert_eq!(
            assembled.identity().map(rift_core::SymbolId::as_str),
            Some(identity.as_str())
        );
        assert_eq!(assembled.index_revision().get(), 7);
        assert_eq!(semantics.graph().records().len(), 1);
    }

    #[test]
    fn compact_facts_build_same_graph_as_complete_document() {
        let document = document();
        let placement = DocumentPlacement::project(&document).expect("project placement");
        let complete = WorkspaceSemantics::build_placed(
            &[PlacedDocument {
                document: &document,
                placement,
            }],
            1,
            7,
            None,
        )
        .expect("complete document semantics");
        let facts = document.shared_facts();
        let compact = WorkspaceSemantics::build_project_facts(
            [(facts.as_ref(), document.path())],
            1,
            7,
            None,
        )
        .expect("compact facts semantics");

        assert_eq!(
            compact.semantics.graph().records(),
            complete.semantics.graph().records()
        );
        assert_eq!(
            compact.beyond_declaration_bound,
            complete.beyond_declaration_bound
        );
    }

    #[test]
    fn zero_revision_has_registered_identity() {
        let error =
            WorkspaceSemantics::build(std::iter::empty(), 1, 0, None).expect_err("zero revision");
        assert_eq!(error.slug().as_str(), "rift.core.revision_zero");
        assert!(!error.to_string().is_empty());
    }

    /// The syntax publication is the only publication a build carries, so the set holds
    /// one provider whatever the documents are.
    #[test]
    fn test_build_publishes_the_syntax_provider_alone() {
        let document = document();
        let semantics = WorkspaceSemantics::build([&document], 1, 3, None)
            .expect("semantics")
            .semantics;
        let syntax =
            rift_core::ProviderId::new(rift_syntax::SYNTAX_PROVIDER_ID).expect("provider identity");
        assert!(semantics.graph().publications().provider(&syntax).is_some());
        assert_eq!(semantics.graph().publications().provider_count(), 1);
    }

    /// The bound is the whole publication's, so the documents that fit are published and
    /// the rest are named. A build that refused the set outright would leave the caller
    /// with no index at all.
    #[test]
    fn test_documents_past_the_declaration_bound_are_named_and_the_rest_publish() {
        let first = declaring("src/first.rs", 3);
        let second = declaring("src/second.rs", 3);
        let third = declaring("src/third.rs", 3);
        let built = WorkspaceSemantics::build([&first, &second, &third], 4, 11, None)
            .expect("the publication keeps the documents that fit");
        assert_eq!(
            built
                .beyond_declaration_bound
                .iter()
                .map(ProjectPath::as_str)
                .collect::<Vec<_>>(),
            ["src/second.rs", "src/third.rs"],
            "the pass stops at the first document that does not fit"
        );
        assert_eq!(built.semantics.graph().records().len(), 3);
    }

    /// A build under the bound names nothing, so the index never leaves a file out for a
    /// bound it did not cross.
    #[test]
    fn test_a_build_within_the_declaration_bound_names_no_document() {
        let document = declaring("src/first.rs", 3);
        let built = WorkspaceSemantics::build([&document], 3, 5, None).expect("semantics");
        assert!(built.beyond_declaration_bound.is_empty());
        assert_eq!(built.semantics.graph().records().len(), 3);
    }

    #[test]
    fn test_zero_declaration_bound_is_a_typed_failure() {
        let error = publication_limits(0).expect_err("a zero bound is refused");
        assert_eq!(
            error.slug().as_str(),
            "rift.provider.publication_zero_limit"
        );
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn test_source_unit_error_keeps_registered_identity() {
        let unit_error = rift_core::SourceUnitId::parse("not-a-source-unit")
            .expect_err("a malformed unit identity is refused");
        assert!(
            unit_error
                .slug()
                .as_str()
                .starts_with("rift.core.source_unit_id_")
        );
        assert!(!unit_error.to_string().is_empty());
    }
}
