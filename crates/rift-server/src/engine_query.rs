//! Complete engine adjacency and physical occurrences for query acquisition.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use lsp_types::{CallHierarchyIncomingCall, CallHierarchyOutgoingCall, Range};
use rift_protocol::query::QueryOccurrence;
use rift_protocol::read::{SourceUnitSpan, TextRange};

use super::{
    CalleeFile, CalleeRoots, CoreSymbolId, EnginePool, EngineReferences, EngineSession, Extensions,
    Instant, NamedCallee, OutgoingAnswer, PositionEncoding, ReadService, ReferenceReport,
    ReferenceTarget, Relationship, RelationshipDerivation, RelationshipFacet, RiftError, SymbolId,
    TraversalDirection, callee_file, errors, project_declaration, reference_source,
    references_on_engine_with_bound, symbol_id,
};
#[cfg(test)]
use super::{IndexedFile, TRAVERSAL_NODES_MAX, resolve_graph_symbol};

#[path = "engine_query_package.rs"]
mod package;
pub use package::{QueryPackageTarget, QuerySourceRequest};

#[path = "engine_query_provider.rs"]
mod provider;
pub(super) use provider::PreparedEdge;
pub(super) use provider::project_provider_edges;

#[derive(Debug)]
enum RetainedTarget {
    Global(Box<super::PackageCallee>),
    Admitted(Arc<QueryPackageTarget>),
}

/// Shared admission for actual provider exchanges within one query.
#[derive(Clone, Debug)]
pub struct ProviderRequests {
    used: Arc<AtomicU64>,
    maximum: u64,
}

impl ProviderRequests {
    /// Starts one shared request count under the accepted query limit.
    #[must_use]
    pub fn new(maximum: u64) -> Self {
        Self {
            used: Arc::new(AtomicU64::new(0)),
            maximum,
        }
    }

    /// Charges one actual exchange before the provider starts it.
    ///
    /// # Errors
    /// Refuses when the accepted request bound is exhausted.
    pub fn claim(&self) -> Result<(), RiftError> {
        self.used.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(1).filter(|used| *used <= self.maximum)
        }).map(|_| ()).map_err(|_| unavailable("query exceeds search.query.requests; narrow the query or increase the accepted bound"))
    }

    /// Actual exchanges still available under the original accepted bound.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.maximum
            .saturating_sub(self.used.load(Ordering::Relaxed))
    }

    /// Reserves a delegated allowance before its boundary request starts.
    ///
    /// An unanswered or canceled delegation keeps its complete reservation.
    ///
    /// # Errors
    /// Refuses an allowance larger than the remaining accepted count.
    pub fn reserve(&self, maximum: u64) -> Result<ProviderRequestAllowance, RiftError> {
        self.used.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(maximum).filter(|used| *used <= self.maximum)
        }).map_err(|_| unavailable("query exceeds search.query.requests; narrow the query or increase the accepted bound"))?;
        Ok(ProviderRequestAllowance {
            requests: self.clone(),
            maximum,
        })
    }
}

/// A delegated count retained until a complete compatible response reports use.
#[derive(Debug)]
pub struct ProviderRequestAllowance {
    requests: ProviderRequests,
    maximum: u64,
}

#[cfg(test)]
mod request_allowance_tests {
    use super::ProviderRequests;

    #[test]
    fn delegated_requests_share_counts_and_keep_failed_work() {
        let requests = ProviderRequests::new(12);
        let first = requests.reserve(8).expect("first delegated allowance");
        requests.claim().expect("first boundary send");
        first.complete(2).expect("validated actual nested use");
        assert_eq!(requests.remaining(), 9);
        let second = requests.reserve(7).expect("second owner allowance");
        requests.claim().expect("second boundary send");
        drop(second);
        assert_eq!(requests.remaining(), 1);
        requests.claim().expect("one retry send remains charged");
        assert!(requests.claim().is_err());
        assert!(requests.reserve(1).is_err());
    }

    #[test]
    fn incompatible_usage_never_releases_reserved_requests() {
        let requests = ProviderRequests::new(4);
        let allowance = requests.reserve(3).expect("delegated allowance");
        requests.claim().expect("boundary send");
        assert!(allowance.complete(4).is_err());
        assert_eq!(requests.remaining(), 0);
    }
}

impl ProviderRequestAllowance {
    /// Returns the count reserved for the delegated owner.
    #[must_use]
    pub const fn maximum(&self) -> u64 {
        self.maximum
    }

    /// Releases only unused requests from a validated complete response.
    ///
    /// # Errors
    /// Refuses reported use beyond the original delegated allowance.
    pub fn complete(self, used: u64) -> Result<(), RiftError> {
        let unused = self
            .maximum
            .checked_sub(used)
            .ok_or_else(|| unavailable("query response exceeds its accepted request allowance"))?;
        self.requests.used.fetch_sub(unused, Ordering::Relaxed);
        Ok(())
    }
}

fn deduplicate_occurrence_records(
    records: &mut Vec<rift_protocol::relationship_read::RelationshipOccurrence>,
) {
    records.sort_by(|left, right| {
        (&left.id, &left.relationship)
            .cmp(&(&right.id, &right.relationship))
            .then_with(|| compare_occurrences(&left.occurrence, &right.occurrence))
    });
    records.dedup();
}

pub(crate) fn merge_package_facts(
    held: &mut rift_search::query::QueryFacts,
    added: rift_search::query::QueryFacts,
) -> Result<(), RiftError> {
    added.validate_providers()?;
    merge_record_evidence(held, added.relationship_records, added.occurrence_records)?;
    let (provider_remap, supplied_associations) = merge_provider_contexts(held, &added.providers)?;
    let source_remap = merge_sources(held, added.sources)?;
    let symbol_remap = merge_symbols(held, added.symbols, &supplied_associations)?;
    merge_provider_snapshots(
        held,
        added.providers,
        &provider_remap,
        &source_remap,
        &symbol_remap,
    )?;
    for mut edge in added.provider_edges {
        for endpoint in edge.from.iter_mut().chain(std::iter::once(&mut edge.to)) {
            if let Some(provider) = endpoint.provider {
                endpoint.provider =
                    Some(*provider_remap.get(provider).ok_or_else(|| {
                        unavailable("captured provider endpoint selector is absent")
                    })?);
            }
        }
        held.provider_edges.push(edge);
    }
    deduplicate_provider_edges(&mut held.provider_edges);
    held.relationships.extend(added.relationships);
    held.occurrences.extend(added.occurrences);
    for source in added.nodes {
        source.validate()?;
        match held
            .nodes
            .iter()
            .find(|previous| previous.unit == source.unit)
        {
            Some(previous) if previous != &source => {
                return unavailable(
                    "captured syntax tree differs within its original source context",
                )
                .fail();
            }
            Some(_) => {}
            None => held.nodes.push(source),
        }
    }
    held.histories.extend(added.histories);
    held.documentation_sources
        .extend(added.documentation_sources);
    held.documentation_blocks.extend(added.documentation_blocks);
    held.documentation_links.extend(added.documentation_links);
    held.documentation_references
        .extend(added.documentation_references);
    held.documentation_reference_candidates
        .extend(added.documentation_reference_candidates);
    for family in added.unavailable_families {
        if !held.unavailable_families.contains(&family) {
            held.unavailable_families.push(family);
        }
    }
    deduplicate_relationships(&mut held.relationships);
    deduplicate_occurrences(&mut held.occurrences);
    Ok(())
}

fn merge_record_evidence(
    held: &mut rift_search::query::QueryFacts,
    relationships: Vec<rift_protocol::relationship_read::RelationshipRecord>,
    occurrences: Vec<rift_protocol::relationship_read::RelationshipOccurrence>,
) -> Result<(), RiftError> {
    let mut records = held
        .relationship_records
        .iter()
        .enumerate()
        .map(|(index, record)| (record.id.clone(), index))
        .collect::<BTreeMap<_, _>>();
    for record in relationships {
        match records.get(&record.id).copied() {
            Some(index) if held.relationship_records[index].relationship != record.relationship => {
                return unavailable(
                    "relationship record identity has conflicting payloads within the selection",
                )
                .fail();
            }
            Some(_) => {}
            None => {
                records.insert(record.id.clone(), held.relationship_records.len());
                held.relationship_records.push(record);
            }
        }
    }
    let mut links = held
        .occurrence_records
        .iter()
        .map(|record| (record.id.clone(), record.relationship.clone()))
        .collect::<BTreeMap<_, _>>();
    for record in occurrences {
        if let Some(previous) = links.insert(record.id.clone(), record.relationship.clone())
            && previous != record.relationship
        {
            return unavailable("original usage record has conflicting relationship links").fail();
        }
        held.occurrence_records.push(record);
    }
    deduplicate_occurrence_records(&mut held.occurrence_records);
    Ok(())
}

fn merge_provider_contexts(
    held: &mut rift_search::query::QueryFacts,
    providers: &[rift_search::query::QueryProviderSnapshot],
) -> Result<(Vec<usize>, BTreeMap<usize, usize>), RiftError> {
    // The array selector scopes original contribution keys to one checked captured graph.
    let mut provider_remap = Vec::with_capacity(providers.len());
    for snapshot in providers {
        let index = if let Some(index) = held
            .providers
            .iter()
            .position(|previous| previous.context == snapshot.context)
        {
            index
        } else {
            let index = held.providers.len();
            held.providers
                .push(rift_search::query::QueryProviderSnapshot {
                    context: snapshot.context.clone(),
                    records: Vec::new(),
                    source_indices: Vec::new(),
                    references: Vec::new(),
                    relationships: Vec::new(),
                });
            index
        };
        provider_remap.push(index);
    }
    let mut associations = BTreeMap::new();
    for (provider, snapshot) in held.providers.iter().enumerate() {
        for record in &snapshot.records {
            for key in &record.contributions {
                associations.insert((provider, key.clone()), record.symbol_index);
            }
        }
    }
    let mut supplied_associations = BTreeMap::new();
    for (provider, snapshot) in providers.iter().enumerate() {
        let provider = provider_remap[provider];
        for record in &snapshot.records {
            for key in &record.contributions {
                if let Some(index) = associations.get(&(provider, key.clone())) {
                    match supplied_associations.insert(record.symbol_index, *index) {
                        Some(previous) if previous != *index => {
                            return unavailable(
                                "captured provider contributions select conflicting records",
                            )
                            .fail();
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok((provider_remap, supplied_associations))
}

fn merge_sources(
    held: &mut rift_search::query::QueryFacts,
    sources: Vec<rift_search::query::QuerySource>,
) -> Result<Vec<usize>, RiftError> {
    let mut source_remap = Vec::with_capacity(sources.len());
    let mut source_units = held
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| (source.unit.clone(), index))
        .collect::<BTreeMap<_, _>>();
    for mut source in sources {
        complete_source_digest(&mut source)?;
        let index = if let Some(index) = source_units.get(&source.unit).copied() {
            let previous = &mut held.sources[index];
            complete_source_digest(previous)?;
            if previous
                .digest
                .as_ref()
                .zip(source.digest.as_ref())
                .is_some_and(|(held, added)| held != added)
            {
                return unavailable("dependency source digest changed during query acquisition")
                    .fail();
            }
            if previous.digest.is_none() {
                previous.digest = source.digest;
            }
            match (&mut previous.text, source.text) {
                (Some(original), Some(captured)) if original != &captured => {
                    return unavailable("dependency source changed during query acquisition")
                        .fail();
                }
                (missing @ None, Some(captured)) => *missing = Some(captured),
                _ => {}
            }
            index
        } else {
            let index = held.sources.len();
            source_units.insert(source.unit.clone(), index);
            held.sources.push(source);
            index
        };
        source_remap.push(index);
    }
    Ok(source_remap)
}

fn merge_symbols(
    held: &mut rift_search::query::QueryFacts,
    symbols: Vec<rift_search::query::QuerySymbol>,
    supplied_associations: &BTreeMap<usize, usize>,
) -> Result<Vec<(usize, Vec<usize>)>, RiftError> {
    let mut identities = held
        .symbols
        .iter()
        .enumerate()
        .filter_map(|(index, symbol)| symbol.symbol.id.clone().map(|identity| (identity, index)))
        .collect::<BTreeMap<_, _>>();
    let mut symbol_remap = Vec::with_capacity(symbols.len());
    for (supplied_index, symbol) in symbols.into_iter().enumerate() {
        let previous = symbol
            .symbol
            .id
            .as_ref()
            .and_then(|identity| identities.get(identity).copied())
            .or_else(|| supplied_associations.get(&supplied_index).copied());
        let (position, declarations) = if let Some(position) = previous {
            let previous = &mut held.symbols[position];
            if previous.symbol != symbol.symbol {
                return unavailable("dependency declarations disagree within the selected source")
                    .fail();
            }
            let mut declarations = Vec::with_capacity(symbol.declarations.len());
            for declaration in symbol.declarations {
                let index = if let Some(index) = previous
                    .declarations
                    .iter()
                    .position(|held| held == &declaration)
                {
                    index
                } else {
                    let index = previous.declarations.len();
                    previous.declarations.push(declaration);
                    index
                };
                declarations.push(index);
            }
            (position, declarations)
        } else {
            let position = held.symbols.len();
            if let Some(identity) = &symbol.symbol.id {
                identities.insert(identity.clone(), position);
            }
            let declarations = (0..symbol.declarations.len()).collect();
            held.symbols.push(symbol);
            (position, declarations)
        };
        symbol_remap.push((position, declarations));
    }
    Ok(symbol_remap)
}

fn merge_provider_snapshots(
    held: &mut rift_search::query::QueryFacts,
    providers: Vec<rift_search::query::QueryProviderSnapshot>,
    provider_remap: &[usize],
    source_remap: &[usize],
    symbol_remap: &[(usize, Vec<usize>)],
) -> Result<(), RiftError> {
    for (supplied_index, mut snapshot) in providers.into_iter().enumerate() {
        let index = provider_remap[supplied_index];
        for source in &mut snapshot.source_indices {
            *source = *source_remap
                .get(*source)
                .ok_or_else(|| unavailable("captured provider source selector is absent"))?;
        }
        for record in &mut snapshot.records {
            let (symbol, declarations) = symbol_remap
                .get(record.symbol_index)
                .ok_or_else(|| unavailable("captured provider symbol selector is absent"))?;
            record.symbol_index = *symbol;
            for declaration in &mut record.declarations {
                declaration.declaration_index = *declarations
                    .get(declaration.declaration_index)
                    .ok_or_else(|| {
                        unavailable("captured provider declaration selector is absent")
                    })?;
            }
        }
        let previous = &mut held.providers[index];
        for source in snapshot.source_indices {
            if !previous.source_indices.contains(&source) {
                previous.source_indices.push(source);
            }
        }
        for record in snapshot.records {
            if let Some(original) = previous.records.iter().find(|original| {
                original
                    .contributions
                    .iter()
                    .any(|key| record.contributions.contains(key))
            }) {
                if original != &record {
                    return unavailable(
                        "captured provider record differs within its original source context",
                    )
                    .fail();
                }
            } else {
                previous.records.push(record);
            }
        }
        if previous.references.is_empty() {
            previous.references = snapshot.references;
        } else if previous.references != snapshot.references {
            return unavailable("captured references differ within their original source context")
                .fail();
        }
        if previous.relationships.is_empty() {
            previous.relationships = snapshot.relationships;
        } else if previous.relationships != snapshot.relationships {
            return unavailable(
                "captured relationships differ within their original source context",
            )
            .fail();
        }
    }
    Ok(())
}

fn deduplicate_provider_edges(edges: &mut Vec<rift_search::query::QueryProviderEdge>) {
    let compare = |left: &rift_search::query::QueryProviderEdge,
                   right: &rift_search::query::QueryProviderEdge| {
        left.from
            .cmp(&right.from)
            .then_with(|| left.to.cmp(&right.to))
            .then_with(|| left.facet.cmp(&right.facet))
            .then_with(|| match (&left.occurrence, &right.occurrence) {
                (Some(left), Some(right)) => left
                    .unit
                    .cmp(&right.unit)
                    .then_with(|| left.range.start.cmp(&right.range.start))
                    .then_with(|| left.range.end.cmp(&right.range.end)),
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
            })
            .then_with(|| left.derivation.cmp(&right.derivation))
    };
    edges.sort_by(compare);
    edges.dedup();
}

fn complete_source_digest(source: &mut rift_search::query::QuerySource) -> Result<(), RiftError> {
    if let Some(text) = &source.text {
        let digest = rift_protocol::identity::SourceDigest::parse(
            &data_encoding::HEXLOWER.encode(rift_core::FileDigest::of(text.as_bytes()).as_bytes()),
        )?;
        if source
            .digest
            .as_ref()
            .is_some_and(|selected| selected != &digest)
        {
            return unavailable("selected dependency source bytes differ from their full digest")
                .fail();
        }
        source.digest = Some(digest);
    }
    Ok(())
}

#[derive(Default)]
struct AcquiredSubject {
    occurrences: Vec<QueryOccurrence>,
    relationships: Vec<Relationship>,
    packages: Vec<super::PackageCallee>,
    sources: Vec<QuerySourceRequest>,
    unconfirmed: Option<String>,
}

/// Request-owned engine facts and admitted dependency targets.
pub struct EngineQuery<'request> {
    query_facts: Option<&'request rift_search::query::QueryFacts>,
    view_input: Option<&'request crate::LocalSymbolViewInput>,
    primary_contexts: Vec<Arc<rift_search::query::QueryProviderContext>>,
    provider_targets: Vec<Arc<QueryPackageTarget>>,
    resolved_providers: BTreeSet<(
        rift_search::query::QueryProviderEndpoint,
        RelationshipFacet,
        bool,
    )>,
    provider_selection: Option<Arc<rift_search::query::QueryProviderSelection>>,
    reads: &'request ReadService,
    engines: &'request EnginePool,
    configuration: &'request rift_protocol::configuration::WorkspaceConfiguration,
    deadline: Instant,
    roots: &'request CalleeRoots,
    references: EngineReferences,
    targets: BTreeMap<CoreSymbolId, RetainedTarget>,
    resolved: BTreeSet<(SymbolId, Option<RelationshipFacet>, bool)>,
    provider_requests: ProviderRequests,
    query_budget: Option<crate::QueryFactBudget<'static>>,
}

impl std::fmt::Debug for EngineQuery<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EngineQuery")
            .field("references", &self.references)
            .field("targets", &self.targets.len())
            .finish_non_exhaustive()
    }
}

impl<'request> EngineQuery<'request> {
    /// Binds acquisition to one selected publication and accepted configuration.
    #[must_use]
    pub fn new(
        reads: &'request ReadService,
        engines: &'request EnginePool,
        configuration: &'request rift_protocol::configuration::WorkspaceConfiguration,
        deadline: Instant,
        roots: &'request CalleeRoots,
    ) -> Self {
        Self {
            provider_targets: Vec::new(),
            query_facts: None,
            view_input: None,
            primary_contexts: Vec::new(),
            resolved_providers: BTreeSet::new(),
            provider_selection: None,
            reads,
            engines,
            configuration,
            deadline,
            roots,
            references: EngineReferences {
                revision: Some(reads.engine_revision().to_owned()),
                ..EngineReferences::default()
            },
            targets: BTreeMap::new(),
            resolved: BTreeSet::new(),
            provider_requests: ProviderRequests::new(configuration.search.query.requests),
            query_budget: None,
        }
    }

    /// Shares the request's retained material admission across provider batches.
    #[must_use]
    pub fn with_query_budget(mut self, budget: crate::QueryFactBudget<'static>) -> Self {
        self.query_budget = Some(budget);
        self
    }

    /// Shares actual request admission with retrieval and delegated provider owners.
    #[must_use]
    pub fn with_provider_requests(mut self, requests: ProviderRequests) -> Self {
        self.provider_requests = requests;
        self
    }

    /// Borrows original primary facts for request-owned provider acquisition.
    ///
    /// # Errors
    /// Refuses metadata copies that exceed the shared request admission.
    pub fn with_query_facts(
        mut self,
        facts: &'request rift_search::query::QueryFacts,
    ) -> Result<Self, RiftError> {
        use get_size2::GetSize as _;
        let mut budget = match &self.query_budget {
            Some(budget) => budget.clone(),
            None => crate::QueryFactBudget::for_request(
                rift_search::query::QueryLimits::try_from(&self.configuration.search.query)?,
                self.deadline.into_std(),
                &|| false,
            )?,
        };
        let slots = facts
            .providers
            .len()
            .checked_mul(std::mem::size_of::<
                Arc<rift_search::query::QueryProviderContext>,
            >())
            .ok_or_else(|| unavailable("the provider directory exceeds the platform bound"))?;
        budget.charge_material(slots)?;
        self.primary_contexts = Vec::with_capacity(facts.providers.len());
        let extra = self
            .primary_contexts
            .capacity()
            .saturating_sub(facts.providers.len())
            .checked_mul(std::mem::size_of::<
                Arc<rift_search::query::QueryProviderContext>,
            >())
            .ok_or_else(|| unavailable("the provider directory exceeds the platform bound"))?;
        budget.charge_material(extra)?;
        for snapshot in &facts.providers {
            budget.charge_material(
                snapshot
                    .context
                    .get_size()
                    .saturating_add(2 * std::mem::size_of::<usize>()),
            )?;
            self.primary_contexts
                .push(Arc::new(snapshot.context.clone()));
        }
        self.query_budget = Some(budget);
        self.query_facts = Some(facts);
        Ok(self)
    }

    /// Binds provider acquisition to the original immutable serving publication.
    #[must_use]
    pub fn with_provider_selection(
        mut self,
        selection: rift_search::query::QueryProviderSelection,
    ) -> Self {
        self.provider_selection = Some(Arc::new(selection));
        self
    }

    /// Borrows the same original material that supplied the selected provider records.
    ///
    /// # Errors
    /// Refuses a different captured serving selection.
    pub fn with_view_input(
        mut self,
        input: &'request crate::LocalSymbolViewInput,
    ) -> Result<Self, RiftError> {
        let selection = self
            .provider_selection
            .as_deref()
            .ok_or_else(|| unavailable("the physical subject has no original serving selection"))?;
        input.validate_provider_selection(selection)?;
        if input.captured_provider_selection() != Some(selection) {
            return unavailable("the physical subject differs from the selected publication")
                .fail();
        }
        self.view_input = Some(input);
        Ok(self)
    }

    /// Original serving selection shared with bounded source preparation.
    #[must_use]
    pub fn provider_selection(&self) -> Option<Arc<rift_search::query::QueryProviderSelection>> {
        self.provider_selection.as_ref().map(Arc::clone)
    }

    /// Shares the same request admission with a provider preparation worker.
    #[must_use]
    pub fn query_budget(&self) -> Option<crate::QueryFactBudget<'static>> {
        self.query_budget.clone()
    }

    /// Complete facts and pending dependency names acquired by this request.
    #[must_use]
    pub const fn references(&self) -> &EngineReferences {
        &self.references
    }

    /// Takes acquired facts after the request no longer needs engine targets.
    #[must_use]
    pub fn into_references(self) -> EngineReferences {
        self.references
    }

    /// Exact workspace and engine roots accepted by this acquisition owner.
    #[must_use]
    pub fn source_trees(&self) -> Vec<std::path::PathBuf> {
        let mut roots = vec![self.reads.index().root().to_path_buf()];
        roots.extend(
            self.engines
                .served_slots()
                .map(|(_, slot)| slot.workspace_root().to_path_buf()),
        );
        roots.sort();
        roots.dedup();
        roots
    }

    /// Adds complete remote adjacency after the client validates its captured selection.
    ///
    /// # Errors
    /// Returns unavailable for a mismatched subject, facet or accepted fact bound.
    pub fn admit_relationships(
        &mut self,
        seed: &SymbolId,
        direction: TraversalDirection,
        relationships: Vec<Relationship>,
        occurrences: Vec<QueryOccurrence>,
    ) -> Result<(), RiftError> {
        let facet = match direction {
            TraversalDirection::Incoming => RelationshipFacet::References,
            TraversalDirection::Outgoing => RelationshipFacet::Calls,
        };
        self.admit_relationships_for_facet(seed, direction, facet, relationships, occurrences)
    }

    /// Adds complete remote adjacency for its selected facet and direction.
    ///
    /// # Errors
    /// Returns unavailable when facts differ from the selected subject or accepted bound.
    pub fn admit_relationships_for_facet(
        &mut self,
        seed: &SymbolId,
        direction: TraversalDirection,
        facet: RelationshipFacet,
        relationships: Vec<Relationship>,
        mut occurrences: Vec<QueryOccurrence>,
    ) -> Result<(), RiftError> {
        let key = (
            seed.clone(),
            Some(facet),
            direction == TraversalDirection::Outgoing,
        );
        if self.relationships_complete(seed, direction, &[facet]) {
            return Ok(());
        }
        for occurrence in &occurrences {
            let anchored = match direction {
                TraversalDirection::Incoming => &occurrence.target == seed,
                TraversalDirection::Outgoing => occurrence.enclosing.as_ref() == Some(seed),
            };
            if !anchored
                || occurrence.facet != facet
                || occurrence.source.range.start > occurrence.source.range.end
            {
                return unavailable("the remote occurrence differs from its selected subject, facet or source range").fail();
            }
        }
        for relationship in &relationships {
            let subject = match direction {
                TraversalDirection::Incoming => &relationship.to,
                TraversalDirection::Outgoing => &relationship.from,
            };
            if subject != seed || !relationship.facets.contains(&facet) {
                return unavailable(
                    "the remote relationship differs from its selected subject or facet",
                )
                .fail();
            }
            if let Some(source) = &relationship.occurrence {
                occurrences.push(QueryOccurrence {
                    target: relationship.to.clone(),
                    enclosing: Some(relationship.from.clone()),
                    source: source.clone(),
                    facet,
                });
            }
        }
        deduplicate_occurrences(&mut occurrences);
        let maximum = usize::try_from(self.configuration.search.query.statements)
            .map_err(|_| unavailable("the accepted query statement bound cannot be represented"))?;
        append_batch(
            &mut self.references,
            AcquiredSubject {
                occurrences,
                relationships,
                ..AcquiredSubject::default()
            },
            maximum,
            self.query_budget.as_ref(),
        )?;
        self.resolved.insert(key);
        self.references.validate_revision(self.reads)
    }

    /// Whether this request already holds complete adjacency for the selection.
    #[must_use]
    pub fn relationships_complete(
        &self,
        seed: &SymbolId,
        direction: TraversalDirection,
        facets: &[RelationshipFacet],
    ) -> bool {
        let outgoing = direction == TraversalDirection::Outgoing;
        self.resolved.contains(&(seed.clone(), None, outgoing))
            || (!facets.is_empty()
                && facets.iter().all(|facet| {
                    self.resolved
                        .contains(&(seed.clone(), Some(*facet), outgoing))
                }))
    }

    /// Retains each original remote record once while preserving physical usages.
    ///
    /// # Errors
    /// Refuses mismatched anchors, facets, record identities or accepted fact bounds.
    pub fn admit_relationship_records(
        &mut self,
        seed: &SymbolId,
        direction: TraversalDirection,
        facets: &[RelationshipFacet],
        records: Vec<rift_protocol::relationship_read::RelationshipRecord>,
        mut occurrences: Vec<rift_protocol::relationship_read::RelationshipOccurrence>,
    ) -> Result<(), RiftError> {
        if self.relationships_complete(seed, direction, facets) {
            return Ok(());
        }
        for record in &records {
            let relationship = &record.relationship;
            let anchor = match direction {
                TraversalDirection::Incoming => &relationship.to,
                TraversalDirection::Outgoing => &relationship.from,
            };
            if anchor != seed
                || (!facets.is_empty()
                    && !relationship
                        .facets
                        .iter()
                        .any(|facet| facets.contains(facet)))
            {
                return unavailable(
                    "the remote relationship differs from its selected subject or facet",
                )
                .fail();
            }
            if let Some(source) = &relationship.occurrence {
                for facet in relationship
                    .facets
                    .iter()
                    .filter(|facet| facets.is_empty() || facets.contains(facet))
                {
                    occurrences.push(rift_protocol::relationship_read::RelationshipOccurrence {
                        id: record.id.clone(),
                        relationship: Some(record.id.clone()),
                        occurrence: QueryOccurrence {
                            target: relationship.to.clone(),
                            enclosing: Some(relationship.from.clone()),
                            source: source.clone(),
                            facet: *facet,
                        },
                    });
                }
            }
        }
        for record in &occurrences {
            let occurrence = &record.occurrence;
            let anchored = match direction {
                TraversalDirection::Incoming => &occurrence.target == seed,
                TraversalDirection::Outgoing => occurrence.enclosing.as_ref() == Some(seed),
            };
            if !anchored
                || (!facets.is_empty() && !facets.contains(&occurrence.facet))
                || occurrence.source.range.start > occurrence.source.range.end
            {
                return unavailable("the remote occurrence differs from its selected subject, facet or source range").fail();
            }
        }
        deduplicate_occurrence_records(&mut occurrences);
        let maximum = usize::try_from(self.configuration.search.query.statements)
            .map_err(|_| unavailable("the accepted query statement bound cannot be represented"))?;
        let count = self
            .references
            .query_package_facts
            .relationship_records
            .len()
            .checked_add(records.len())
            .and_then(|count| {
                count.checked_add(self.references.query_package_facts.occurrence_records.len())
            })
            .and_then(|count| count.checked_add(occurrences.len()));
        if count.is_none_or(|count| count > maximum) {
            return unavailable("query exceeds search.query.statements; narrow the query or increase the accepted bound").fail();
        }
        self.admit_package_facts(rift_search::query::QueryFacts {
            relationship_records: records,
            occurrence_records: occurrences,
            ..rift_search::query::QueryFacts::default()
        })?;
        let outgoing = direction == TraversalDirection::Outgoing;
        if facets.is_empty() {
            self.resolved.insert((seed.clone(), None, outgoing));
        }
        for facet in facets {
            self.resolved.insert((seed.clone(), Some(*facet), outgoing));
        }
        self.references.validate_revision(self.reads)
    }

    /// Retains exact package declarations and source bytes selected by this request.
    ///
    /// # Errors
    /// Returns unavailable for conflicting facts or the accepted retained byte bound.
    pub fn admit_package_facts(
        &mut self,
        facts: rift_search::query::QueryFacts,
    ) -> Result<(), RiftError> {
        let incoming_bytes = retained_package_bytes(&facts);
        if let Some(budget) = &mut self.query_budget {
            budget.charge_material(incoming_bytes)?;
        }
        let bytes = retained_package_bytes(&self.references.query_package_facts)
            .checked_add(incoming_bytes);
        if bytes
            .is_none_or(|bytes| bytes as u64 > self.configuration.search.query.fact_size.bytes())
        {
            return unavailable("query exceeds search.query.fact_size; narrow the query or increase the accepted bound").fail();
        }
        merge_package_facts(&mut self.references.query_package_facts, facts)
    }

    /// Charges one provider exchange against the accepted request bound.
    ///
    /// # Errors
    /// Returns unavailable when the accepted request bound is exhausted.
    pub fn claim_provider_request(&self) -> Result<(), RiftError> {
        self.provider_requests.claim()
    }

    /// Provider exchanges still available within this request's accepted bound.
    #[must_use]
    pub fn remaining_provider_requests(&self) -> u64 {
        self.provider_requests.remaining()
    }

    /// Shares this query's existing admission with another provider transport.
    #[must_use]
    pub fn provider_requests(&self) -> ProviderRequests {
        self.provider_requests.clone()
    }

    /// Sources awaiting explicit provider selection and bounded worker preparation.
    #[must_use]
    pub fn source_requests(&self) -> Vec<QuerySourceRequest> {
        let requests = self
            .references
            .package_callees
            .iter()
            .cloned()
            .map(QuerySourceRequest::Call)
            .chain(self.references.query_source_requests.iter().cloned());
        let mut sources = BTreeSet::new();
        requests
            .filter(|request| {
                let (uri, provider) = match request {
                    QuerySourceRequest::Call(callee)
                    | QuerySourceRequest::IncomingCall { caller: callee, .. } => {
                        (callee.query_uri(), callee.query_provider())
                    }
                    QuerySourceRequest::Reference { uri, provider, .. }
                    | QuerySourceRequest::Provider { uri, provider, .. } => {
                        (Some(uri), Some(provider))
                    }
                };
                sources.insert((
                    uri.map(|uri| uri.as_str().to_owned()),
                    provider.map(|provider| (provider.name.clone(), provider.dialect.clone())),
                ))
            })
            .collect()
    }

    /// Unique admitted dependency sources requiring final worker validation.
    #[must_use]
    pub fn validation_targets(&self) -> Vec<Arc<QueryPackageTarget>> {
        let mut targets: Vec<_> = self
            .targets
            .values()
            .filter_map(|target| {
                let RetainedTarget::Admitted(target) = target else {
                    return None;
                };
                Some(Arc::clone(target))
            })
            .collect();
        targets.extend(self.provider_targets.iter().map(Arc::clone));
        let mut seen = BTreeSet::new();
        let mut held: Vec<Arc<QueryPackageTarget>> = Vec::new();
        for target in targets {
            if seen.insert(Arc::as_ptr(&target)) {
                held.push(target);
            }
        }
        held
    }

    /// Adds facts prepared from admitted bytes before resuming the engine acquisition.
    ///
    /// # Errors
    /// Returns unavailable when an admitted source differs from a retained source unit.
    pub fn admit_package_target(
        &mut self,
        mut target: QueryPackageTarget,
    ) -> Result<(), RiftError> {
        let named_global = match &target.request {
            QuerySourceRequest::Call(callee) => self.targets.iter().find_map(|(identity, held)| {
                matches!(held, RetainedTarget::Global(previous) if previous.as_ref() == callee)
                    .then_some(identity)
            }),
            QuerySourceRequest::Reference { .. }
            | QuerySourceRequest::IncomingCall { .. }
            | QuerySourceRequest::Provider { .. } => None,
        };
        if named_global.is_some_and(|identity| {
            target.selected.as_ref().map(|selected| selected.0.as_str()) != Some(identity.as_str())
        }) {
            return unavailable(
                "dependency source declaration differs from the selected global identity",
            )
            .fail();
        }
        let replaces_global = named_global.is_some();
        let incoming_bytes = retained_package_bytes(&target.facts);
        if let Some(budget) = &mut self.query_budget {
            budget.charge_material(incoming_bytes)?;
        }
        let bytes = retained_package_bytes(&self.references.query_package_facts)
            .checked_add(incoming_bytes);
        if bytes
            .is_none_or(|bytes| bytes as u64 > self.configuration.search.query.fact_size.bytes())
        {
            return unavailable("query exceeds search.query.fact_size; narrow the query or increase the accepted bound").fail();
        }
        let facts = std::mem::take(&mut target.facts);
        let pending_before = self
            .references
            .package_callees
            .len()
            .saturating_add(self.references.query_source_requests.len());
        let physical_edges = self.name_physical_package_sources(&target, &facts)?;
        self.name_incoming_package_requests(&facts)?;
        self.name_prepared_package_requests(&target, &facts)?;
        let pending_after = self
            .references
            .package_callees
            .len()
            .saturating_add(self.references.query_source_requests.len());
        if pending_after >= pending_before && !replaces_global {
            return unavailable("the prepared source did not resolve a pending dependency fact")
                .fail();
        }
        deduplicate_occurrences(&mut self.references.query_occurrences);
        deduplicate_relationships(&mut self.references.query_relationships);
        merge_package_facts(&mut self.references.query_package_facts, facts)?;
        self.admit_prepared_edges(physical_edges)?;
        let held = Arc::new(target);
        self.provider_targets.push(Arc::clone(&held));
        for identity in held.name_ranges.keys() {
            let identity = CoreSymbolId::new(identity.0.clone())?;
            self.targets
                .insert(identity, RetainedTarget::Admitted(Arc::clone(&held)));
        }
        Ok(())
    }

    fn name_incoming_package_requests(
        &mut self,
        facts: &rift_search::query::QueryFacts,
    ) -> Result<(), RiftError> {
        let mut waiting_calls = Vec::new();
        let mut caller_positions = BTreeMap::new();
        for request in std::mem::take(&mut self.references.query_source_requests) {
            if let QuerySourceRequest::IncomingCall {
                target: called,
                caller,
                ranges,
            } = &request
                && let Some(identity) = known_package_symbol(caller, facts)?
            {
                let origin = caller
                    .query_origin()
                    .ok_or_else(|| unavailable("the caller has no exact dependency owner"))?;
                let unit = caller
                    .unit(&origin)
                    .ok_or_else(|| unavailable("the caller has no exact source unit"))?;
                let text = QueryPackageTarget::source_for(facts, &unit)?;
                let positions = caller_positions
                    .entry(unit.clone())
                    .or_insert_with(|| rift_lsp::position::LineIndex::new(text));
                let sources = ranges
                    .iter()
                    .map(|range| {
                        indexed_range(positions, *range, caller.encoding()).map(|range| {
                            SourceUnitSpan {
                                unit: unit.clone(),
                                range,
                            }
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                name_package_occurrences(
                    &mut self.references.query_occurrences,
                    &mut self.references.query_relationships,
                    &identity,
                    called,
                    &sources,
                );
            } else {
                waiting_calls.push(request);
            }
        }
        drop(caller_positions);
        self.references.query_source_requests = waiting_calls;
        Ok(())
    }

    fn name_prepared_package_requests(
        &mut self,
        target: &QueryPackageTarget,
        facts: &rift_search::query::QueryFacts,
    ) -> Result<(), RiftError> {
        match &target.request {
            QuerySourceRequest::Provider { .. } | QuerySourceRequest::IncomingCall { .. } => {}
            QuerySourceRequest::Call(callee) => {
                let mut waiting = Vec::new();
                for pending in std::mem::take(&mut self.references.package_callees) {
                    if pending.query_uri() == callee.query_uri()
                        && pending.package() == callee.package()
                        && let Some(identity) = known_package_symbol(&pending, facts)?
                    {
                        name_package_occurrences(
                            &mut self.references.query_occurrences,
                            &mut self.references.query_relationships,
                            &SymbolId(pending.caller().as_str().to_owned()),
                            &identity,
                            pending.query_sources().unwrap_or_default(),
                        );
                    } else {
                        waiting.push(pending);
                    }
                }
                self.references.package_callees = waiting;
            }
            QuerySourceRequest::Reference { uri, .. } => {
                let source = facts
                    .sources
                    .iter()
                    .find(|source| source.unit == target.unit)
                    .and_then(|source| source.text.as_deref())
                    .ok_or_else(|| {
                        unavailable("the prepared dependency source has no selected bytes")
                    })?;
                let mut waiting = Vec::new();
                let positions = rift_lsp::position::LineIndex::new(source);
                for request in std::mem::take(&mut self.references.query_source_requests) {
                    let QuerySourceRequest::Reference {
                        target: symbol,
                        uri: requested_uri,
                        range,
                        encoding,
                        facet,
                        file_level,
                        ..
                    } = &request
                    else {
                        waiting.push(request);
                        continue;
                    };
                    if requested_uri != uri {
                        waiting.push(request);
                        continue;
                    }
                    let range = indexed_range(&positions, *range, *encoding)?;
                    if *facet == RelationshipFacet::References
                        && target.name_ranges.get(symbol).is_some_and(|location| {
                            location.source.unit == target.unit && location.source.range == range
                        })
                    {
                        continue;
                    }
                    let occurrence = QueryOccurrence {
                        target: symbol.clone(),
                        enclosing: (!*file_level)
                            .then(|| enclosing_package_symbol(facts, &target.unit, &range))
                            .flatten(),
                        source: SourceUnitSpan {
                            unit: target.unit.clone(),
                            range,
                        },
                        facet: *facet,
                    };
                    self.references
                        .query_relationships
                        .extend(relationship(&occurrence));
                    self.references.query_occurrences.push(occurrence);
                }
                self.references.query_source_requests = waiting;
            }
        }
        Ok(())
    }

    /// Existing naming operations for facts retained by this request.
    pub fn references_mut(&mut self) -> &mut EngineReferences {
        &mut self.references
    }

    /// Canonical identity already selected by an upstream declaration read.
    ///
    /// Physical engine locations do not require namespace enrichment merely to retain a target.
    #[must_use]
    pub fn package_identity(&self, request: &QuerySourceRequest) -> Option<SymbolId> {
        let QuerySourceRequest::Call(callee) = request else {
            return None;
        };
        self.targets.iter().find_map(|(identity, held)| {
            matches!(held, RetainedTarget::Global(previous) if previous.as_ref() == callee)
                .then(|| SymbolId(identity.as_str().to_owned()))
        })
    }

    /// Admitted source target retained when the global API named a declaration.
    #[must_use]
    pub fn named_package_target(&self, identity: &SymbolId) -> Option<&super::PackageCallee> {
        let identity = CoreSymbolId::new(identity.0.clone()).ok()?;
        match self.targets.get(&identity)? {
            RetainedTarget::Global(callee) => Some(callee),
            RetainedTarget::Admitted(_) => None,
        }
    }

    /// Names pending dependency calls while retaining their admitted engine targets.
    pub fn name_package_callees(
        &mut self,
        mut named: impl FnMut(&super::PackageCallee) -> Option<crate::CalleeDeclaration>,
    ) {
        let targets = &mut self.targets;
        self.references.name_package_callees(|callee| {
            let declaration = named(callee)?;
            if let Ok(identity) = CoreSymbolId::new(declaration.id.0.clone()) {
                targets.insert(identity, RetainedTarget::Global(Box::new(callee.clone())));
            }
            Some(declaration)
        });
    }

    /// Acquires complete one-hop facts for subjects not previously requested here.
    ///
    /// # Errors
    /// Returns typed read failures for unavailable or incomplete engine contributions.
    ///
    /// # Cancel safety
    /// Dropping the future leaves cleanup with the owning engine exchange.
    pub async fn resolve(
        &mut self,
        seeds: &[SymbolId],
        direction: TraversalDirection,
    ) -> Result<(), RiftError> {
        let facet = match direction {
            TraversalDirection::Incoming => RelationshipFacet::References,
            TraversalDirection::Outgoing => RelationshipFacet::Calls,
        };
        Box::pin(self.resolve_facet(seeds, direction, facet)).await
    }

    async fn resolve_facet(
        &mut self,
        seeds: &[SymbolId],
        direction: TraversalDirection,
        facet: RelationshipFacet,
    ) -> Result<(), RiftError> {
        let maximum = usize::try_from(self.configuration.search.query.statements)
            .map_err(|_| unavailable("the accepted query statement bound cannot be represented"))?;
        for seed in seeds {
            let key = (
                seed.clone(),
                Some(facet),
                direction == TraversalDirection::Outgoing,
            );
            if self.relationships_complete(seed, direction, &[facet]) {
                continue;
            }
            let identity = CoreSymbolId::new(seed.0.clone()).map_err(|_| {
                errors::server::read_invalid()
                    .field("query")
                    .violation("the relationship subject must be a canonical symbol identity")
                    .error()
            })?;
            let acquisition = Acquisition {
                reads: self.reads,
                view_input: self.view_input,
                engines: self.engines,
                deadline: self.deadline,
                roots: self.roots,
                facts: &self.references.query_package_facts,
                maximum,
                requests: self.provider_requests.clone(),
            };
            let batch = match self.targets.get(&identity) {
                Some(RetainedTarget::Admitted(target)) => {
                    Box::pin(acquisition.dependency(&identity, target, direction, facet)).await?
                }
                Some(RetainedTarget::Global(_)) => {
                    return unavailable(
                        "the selected dependency subject requires global relationship acquisition",
                    )
                    .fail();
                }
                None => Box::pin(acquisition.project(&identity, direction, facet)).await?,
            };
            append_batch(
                &mut self.references,
                batch,
                maximum,
                self.query_budget.as_ref(),
            )?;
            self.resolved.insert(key);
        }
        self.references.validate_revision(self.reads)
    }

    /// Acquires only relationships whose direction the selected provider establishes.
    ///
    /// # Errors
    /// Returns the registered capability refusal for an unsupported required relationship.
    ///
    /// # Cancel safety
    /// Dropping the future preserves the owning engine exchange cleanup.
    pub async fn resolve_facets(
        &mut self,
        seeds: &[SymbolId],
        direction: TraversalDirection,
        facets: &[RelationshipFacet],
    ) -> Result<(), RiftError> {
        if facets.is_empty()
            || facets.iter().any(|facet| {
                !matches!(
                    (direction, facet),
                    (
                        TraversalDirection::Incoming,
                        RelationshipFacet::References | RelationshipFacet::Calls
                    ) | (TraversalDirection::Outgoing, RelationshipFacet::Calls)
                )
            })
        {
            return errors::server::read_unsupported()
                .capability("the required relationship and direction have no complete language engine provider")
                .fail();
        }
        for facet in facets {
            Box::pin(self.resolve_facet(seeds, direction, *facet)).await?;
        }
        Ok(())
    }
}

/// Acquires complete one-hop engine facts for the selected subjects.
///
/// Every request retains physical occurrences before projecting declaration edges.
/// The existing engine response bound applies across the batch. Missing capabilities,
/// unavailable source and incomplete responses refuse the complete batch.
///
/// # Errors
///
/// Returns typed read failures for incomplete acquisition and invalid subjects.
///
/// # Cancel safety
///
/// Dropping the future leaves cleanup with the owning engine exchange.
pub async fn resolve_query_references(
    reads: &ReadService,
    engines: &EnginePool,
    seeds: &[SymbolId],
    direction: TraversalDirection,
    deadline: Instant,
    roots: &CalleeRoots,
    configuration: &rift_protocol::configuration::WorkspaceConfiguration,
) -> Result<EngineReferences, RiftError> {
    let mut query = EngineQuery::new(reads, engines, configuration, deadline, roots);
    query.resolve(seeds, direction).await?;
    Ok(query.references)
}

fn unavailable(detail: impl Into<String>) -> RiftError {
    errors::server::read_unavailable()
        .operation("query relationships")
        .detail(detail.into())
        .error()
}

fn append_batch(
    result: &mut EngineReferences,
    batch: AcquiredSubject,
    maximum: usize,
    budget: Option<&crate::QueryFactBudget<'static>>,
) -> Result<(), RiftError> {
    let counts = [
        result.query_occurrences.len(),
        result.query_relationships.len(),
        result.package_callees.len(),
        result.query_source_requests.len(),
        batch.occurrences.len(),
        batch.relationships.len(),
        batch.packages.len(),
        batch.sources.len(),
    ];
    let count = counts.into_iter().try_fold(0usize, usize::checked_add);
    if count.is_none_or(|count| count > maximum) {
        return unavailable("query exceeds search.query.statements; narrow the query or increase the accepted bound").fail();
    }
    if let Some(budget) = budget {
        let mut budget = budget.clone();
        let (relationships, _) = rift_protocol::retained_vector_material(
            &batch.relationships,
            get_size2::StandardTracker::default(),
        );
        budget.charge_material(relationships)?;
        budget.charge_material(retained_occurrence_bytes(&batch.occurrences))?;
    }
    result.query_relationships.extend(batch.relationships);
    result.query_occurrences.extend(batch.occurrences);
    result.package_callees.extend(batch.packages);
    result.query_source_requests.extend(batch.sources);
    result.unconfirmed.extend(batch.unconfirmed);
    Ok(())
}

struct Acquisition<'source> {
    reads: &'source ReadService,
    view_input: Option<&'source crate::LocalSymbolViewInput>,
    engines: &'source EnginePool,
    deadline: Instant,
    roots: &'source CalleeRoots,
    facts: &'source rift_search::query::QueryFacts,
    maximum: usize,
    requests: ProviderRequests,
}

impl<'source> Acquisition<'source> {
    fn reference_source(
        &self,
        identity: &CoreSymbolId,
    ) -> Result<Option<super::ReferenceSource<'source>>, RiftError> {
        if let Some(input) = self.view_input {
            for index in input.indices() {
                if let Some(binding) = index.canonical_declarations(identity).next() {
                    let (_, _, matched, _) = binding?;
                    if matched.symbol.name_range.is_none() {
                        continue;
                    }
                    let Some(slot) = self.engines.engine_for(matched.file.syntax().language())
                    else {
                        continue;
                    };
                    return Ok(Some((slot, matched.file, matched.symbol)));
                }
            }
            return Ok(None);
        }
        reference_source(self.reads, self.engines, identity)
    }

    async fn project(
        self,
        identity: &CoreSymbolId,
        direction: TraversalDirection,
        facet: RelationshipFacet,
    ) -> Result<AcquiredSubject, RiftError> {
        let Some((slot, file, symbol)) = self.reference_source(identity)? else {
            if let Some((file, _)) = super::resolve_graph_symbol(self.reads.index(), identity)?
                && self.engines.engine_for(file.syntax().language()).is_none()
            {
                return errors::server::read_unsupported()
                    .capability(
                        "relationship acquisition (no language engine serves the selected subject)",
                    )
                    .fail();
            }
            return unavailable(format!(
                "the selected subject {identity} has no admitted engine source"
            ))
            .fail();
        };
        let target = ReferenceTarget::new(
            slot.workspace_root(),
            self.reads.index().root(),
            file,
            symbol,
        )?;
        if direction == TraversalDirection::Incoming && facet == RelationshipFacet::Calls {
            return self
                .incoming_calls(identity, slot, &target, file.syntax().language())
                .await;
        }
        match direction {
            TraversalDirection::Incoming => {
                let (report, unconfirmed) = Box::pin(references_on_engine_with_bound(
                    slot,
                    &target,
                    self.deadline,
                    self.maximum,
                    Some(self.requests.clone()),
                ))
                .await?;
                let excluded = symbol.name_range.map(|range| {
                    (
                        crate::read::source_unit_id(file.path()),
                        TextRange {
                            start: range.start,
                            end: range.end,
                        },
                    )
                });
                let mut batch = map_complete_references(
                    self.reads,
                    identity,
                    report,
                    slot.workspace_root(),
                    self.roots,
                    self.facts,
                    (file.syntax().language(), excluded),
                )?;
                batch.unconfirmed = unconfirmed.then(|| target.language.clone());
                Ok(batch)
            }
            TraversalDirection::Outgoing => {
                let (report, unconfirmed) = outgoing_report(
                    Box::pin(outgoing_on_engine(
                        slot,
                        &target,
                        self.deadline,
                        self.maximum,
                        self.requests.clone(),
                    ))
                    .await?,
                )?;
                let source = CallSource {
                    text: file.source(),
                    unit: crate::read::source_unit_id(file.path()),
                    provider: file.syntax().language(),
                };
                let mut batch = map_source_calls(
                    self.reads,
                    identity,
                    &source,
                    report,
                    self.roots,
                    slot.workspace_root(),
                    self.facts,
                )?;
                batch.unconfirmed = unconfirmed.then(|| target.language.clone());
                Ok(batch)
            }
        }
    }

    async fn dependency(
        self,
        identity: &CoreSymbolId,
        held: &QueryPackageTarget,
        direction: TraversalDirection,
        facet: RelationshipFacet,
    ) -> Result<AcquiredSubject, RiftError> {
        let slot = self
            .engines
            .engine_for(&held.provider)
            .ok_or_else(|| unavailable("the admitted dependency provider is unavailable"))?;
        let id = SymbolId(identity.as_str().to_owned());
        let declaration = held.name_ranges.get(&id).ok_or_else(|| {
            unavailable("the admitted dependency has no declaration name position")
        })?;
        let offset = declaration.source.range.start;
        let source = QueryPackageTarget::source_for(self.facts, &declaration.source.unit)?;
        let index = rift_lsp::position::LineIndex::new(source);
        let offset = usize::try_from(offset).map_err(|_| {
            unavailable("the dependency declaration position cannot be represented")
        })?;
        let target = ReferenceTarget {
            language: held.provider.name.clone(),
            source: source.to_owned(),
            utf8: index.position(PositionEncoding::Utf8, offset)?,
            utf16: index.position(PositionEncoding::Utf16, offset)?,
            uri: declaration.uri.clone(),
            canonical_uri: declaration.uri.clone(),
        };
        if direction == TraversalDirection::Incoming && facet == RelationshipFacet::Calls {
            return self
                .incoming_calls(identity, slot, &target, &held.provider)
                .await;
        }
        match direction {
            TraversalDirection::Incoming => {
                let (report, unconfirmed) = Box::pin(references_on_engine_with_bound(
                    slot,
                    &target,
                    self.deadline,
                    self.maximum,
                    Some(self.requests.clone()),
                ))
                .await?;
                let exclusion = held
                    .name_ranges
                    .get(&id)
                    .map(|location| (location.source.unit.clone(), location.source.range.clone()));
                let mut batch = map_complete_references(
                    self.reads,
                    identity,
                    report,
                    slot.workspace_root(),
                    self.roots,
                    self.facts,
                    (&held.provider, exclusion),
                )?;
                batch.unconfirmed = unconfirmed.then(|| target.language.clone());
                Ok(batch)
            }
            TraversalDirection::Outgoing => {
                let (report, unconfirmed) = outgoing_report(
                    Box::pin(outgoing_on_engine(
                        slot,
                        &target,
                        self.deadline,
                        self.maximum,
                        self.requests.clone(),
                    ))
                    .await?,
                )?;
                let source = CallSource {
                    text: source,
                    unit: declaration.source.unit.clone(),
                    provider: &held.provider,
                };
                let mut batch = map_source_calls(
                    self.reads,
                    identity,
                    &source,
                    report,
                    self.roots,
                    slot.workspace_root(),
                    self.facts,
                )?;
                batch.unconfirmed = unconfirmed.then(|| target.language.clone());
                Ok(batch)
            }
        }
    }
    async fn incoming_calls(
        self,
        identity: &CoreSymbolId,
        slot: &super::EngineSlot,
        target: &ReferenceTarget,
        provider: &rift_protocol::read::Language,
    ) -> Result<AcquiredSubject, RiftError> {
        let (report, unconfirmed) = outgoing_report(
            Box::pin(incoming_on_engine(
                slot,
                target,
                self.deadline,
                self.maximum,
                self.requests,
            ))
            .await?,
        )?;
        let mut batch = map_incoming_calls(
            self.reads,
            identity,
            report,
            self.roots,
            slot.workspace_root(),
            self.facts,
            provider,
        )?;
        batch.unconfirmed = unconfirmed.then(|| target.language.clone());
        Ok(batch)
    }
}

fn outgoing_report<T>(answer: OutgoingAnswer<T>) -> Result<(T, bool), RiftError> {
    match answer {
        OutgoingAnswer::Ready(report) => Ok((report, false)),
        OutgoingAnswer::Unconfirmed(report) => Ok((report, true)),
        OutgoingAnswer::Unprepared => {
            unavailable("the selected engine prepared no call hierarchy item").fail()
        }
        OutgoingAnswer::Unsettled { .. } => {
            unavailable("the selected engine did not finish its readiness wait").fail()
        }
    }
}

fn map_complete_references(
    reads: &ReadService,
    target: &CoreSymbolId,
    report: ReferenceReport,
    workspace_root: &std::path::Path,
    roots: &CalleeRoots,
    facts: &rift_search::query::QueryFacts,
    selection: (
        &rift_protocol::read::Language,
        Option<(rift_protocol::read::SourceUnitId, TextRange)>,
    ),
) -> Result<AcquiredSubject, RiftError> {
    let (provider, excluded) = selection;
    if !report.full {
        return unavailable("the reference answer did not establish the declaration").fail();
    }
    let locations = report
        .locations
        .map_err(|_| unavailable("the reference answer exceeds the engine response bound"))?;
    let trees = [workspace_root, reads.index().root()];
    let mut acquired = AcquiredSubject::default();
    let mut positions = BTreeMap::new();
    for location in locations {
        let address = roots.address(&trees, &location.uri)?;
        let occurrence = match address {
            Some(rift_lsp::uri::EngineAddress::Project(path)) => {
                let file = reads.index().file(&path).ok_or_else(|| {
                    unavailable("an engine reference has no selected source bytes")
                })?;
                let unit = crate::read::source_unit_id(file.path());
                let index = positions
                    .entry(unit.clone())
                    .or_insert_with(|| rift_lsp::position::LineIndex::new(file.source()));
                let range = indexed_range(index, location.range, report.encoding)?;
                let enclosing = file
                    .enclosing_symbol(range.start, range.end)
                    .map(|symbol| symbol_id(reads.index(), crate::search::declared(file, symbol)))
                    .transpose()?
                    .flatten();
                QueryOccurrence {
                    target: SymbolId(target.as_str().to_owned()),
                    enclosing,
                    source: SourceUnitSpan { unit, range },
                    facet: RelationshipFacet::References,
                }
            }
            Some(rift_lsp::uri::EngineAddress::Package(file)) => {
                let unit = rift_protocol::read::SourceUnitId(file.unit().to_string());
                let source = facts
                    .sources
                    .iter()
                    .find(|source| source.unit == unit)
                    .and_then(|source| source.text.as_deref());
                let Some(source) = source else {
                    acquired.sources.push(QuerySourceRequest::Reference {
                        target: SymbolId(target.as_str().to_owned()),
                        uri: location.uri,
                        range: location.range,
                        encoding: report.encoding,
                        provider: provider.clone(),
                        facet: RelationshipFacet::References,
                        file_level: false,
                    });
                    continue;
                };
                let index = positions
                    .entry(unit.clone())
                    .or_insert_with(|| rift_lsp::position::LineIndex::new(source));
                let range = indexed_range(index, location.range, report.encoding)?;
                QueryOccurrence {
                    target: SymbolId(target.as_str().to_owned()),
                    enclosing: enclosing_package_symbol(facts, &unit, &range),
                    source: SourceUnitSpan { unit, range },
                    facet: RelationshipFacet::References,
                }
            }
            None => return unavailable("an engine reference has no admitted source owner").fail(),
        };
        let declaration = excluded.as_ref().is_some_and(|(unit, range)| {
            occurrence.source.unit == *unit && occurrence.source.range == *range
        });
        if declaration {
            continue;
        }
        acquired.occurrences.push(occurrence);
    }
    deduplicate_occurrences(&mut acquired.occurrences);
    acquired.relationships = acquired
        .occurrences
        .iter()
        .filter_map(relationship)
        .collect();
    Ok(acquired)
}

fn enclosing_package_symbol(
    facts: &rift_search::query::QueryFacts,
    unit: &rift_protocol::read::SourceUnitId,
    range: &TextRange,
) -> Option<SymbolId> {
    facts
        .symbols
        .iter()
        .flat_map(|symbol| {
            symbol
                .declarations
                .iter()
                .map(move |declaration| (symbol, declaration))
        })
        .filter(|(_, declaration)| {
            declaration.unit.as_ref() == Some(unit)
                && declaration.range.start <= range.start
                && range.end <= declaration.range.end
        })
        .min_by_key(|(_, declaration)| declaration.range.end - declaration.range.start)
        .and_then(|(symbol, _)| symbol.symbol.id.clone())
}

struct IncomingReport {
    maximum: usize,
    calls: Vec<CallHierarchyIncomingCall>,
    encoding: PositionEncoding,
}

impl IncomingReport {
    fn validate_count(&self) -> Result<(), RiftError> {
        let count = self.calls.iter().try_fold(0_usize, |count, call| {
            count.checked_add(call.from_ranges.len().max(1))
        });
        if count.is_none_or(|count| count > self.maximum) {
            return unavailable("the incoming call occurrences exceed the engine response bound")
                .fail();
        }
        Ok(())
    }
}

async fn incoming_on_engine(
    slot: &super::EngineSlot,
    target: &ReferenceTarget,
    deadline: Instant,
    maximum: usize,
    requests: ProviderRequests,
) -> Result<OutgoingAnswer<IncomingReport>, RiftError> {
    let uri = target.uri.clone();
    let utf8 = target.utf8;
    let utf16 = target.utf16;
    slot.request_outgoing(
        target.opening(),
        move |session: &mut EngineSession| {
            let uri = uri.clone();
            let requests = requests.clone();
            Box::pin(async move {
                let encoding = session.capabilities().position_encoding;
                let position = match encoding {
                    PositionEncoding::Utf8 => utf8,
                    PositionEncoding::Utf16 => utf16,
                };
                requests.claim()?;
                let items = session.prepare_call_hierarchy_uri(&uri, position).await?;
                if items.is_empty() {
                    return Ok(None);
                }
                let mut calls = Vec::new();
                for item in items {
                    requests.claim()?;
                    calls.extend(session.incoming_calls(item).await?);
                    if calls.len() > maximum {
                        return unavailable(
                            "the incoming call answer exceeds the engine response bound",
                        )
                        .fail();
                    }
                }
                Ok(Some(IncomingReport {
                    maximum,
                    calls,
                    encoding,
                }))
            })
        },
        target.closing(),
        deadline,
    )
    .await
}

fn map_incoming_calls(
    reads: &ReadService,
    target: &CoreSymbolId,
    report: IncomingReport,
    roots: &CalleeRoots,
    workspace_root: &std::path::Path,
    facts: &rift_search::query::QueryFacts,
    provider: &rift_protocol::read::Language,
) -> Result<AcquiredSubject, RiftError> {
    report.validate_count()?;
    let trees = [workspace_root, reads.index().root()];
    let mut acquired = AcquiredSubject::default();
    let subject = SymbolId(target.as_str().to_owned());
    let mut caller_positions = BTreeMap::new();
    for call in report.calls {
        let file_level = call.from.kind == lsp_types::SymbolKind::MODULE
            && call.from.range == Range::default()
            && call.from.selection_range == Range::default();
        let named = NamedCallee {
            uri: &call.from.uri,
            name: &call.from.name,
            position: call.from.selection_range.start,
            encoding: report.encoding,
        };
        let (caller, unit, text) = match callee_file(roots, &trees, target, named)? {
            CalleeFile::Project(path) => {
                let caller = if file_level {
                    None
                } else {
                    Some(
                        project_declaration(
                            reads,
                            &path,
                            (call.from.selection_range, report.encoding),
                            "query calls",
                        )?
                        .ok_or_else(|| {
                            unavailable("the incoming caller has no selected declaration")
                        })?,
                    )
                };
                let file = reads.index().file(&path).ok_or_else(|| {
                    unavailable("the incoming caller has no selected source bytes")
                })?;
                (
                    caller,
                    crate::read::source_unit_id(file.path()),
                    file.source(),
                )
            }
            CalleeFile::Package(dependency) => {
                let caller = if file_level {
                    None
                } else if let Some(caller) = known_package_symbol(&dependency, facts)? {
                    Some(caller)
                } else {
                    acquired.sources.push(QuerySourceRequest::IncomingCall {
                        target: subject.clone(),
                        caller: dependency.with_query_sources(Vec::new(), provider.clone()),
                        ranges: call.from_ranges,
                    });
                    continue;
                };
                let origin = dependency
                    .query_origin()
                    .ok_or_else(|| unavailable("the incoming caller has no exact owner"))?;
                let unit = dependency
                    .unit(&origin)
                    .ok_or_else(|| unavailable("the incoming caller has no source unit"))?;
                let text = facts
                    .sources
                    .iter()
                    .find(|source| source.unit == unit)
                    .and_then(|source| source.text.as_deref());
                let Some(text) = text else {
                    if file_level {
                        retain_incoming_file_references(
                            &mut acquired,
                            &subject,
                            call,
                            report.encoding,
                            provider,
                        );
                        continue;
                    }
                    return Err(unavailable(
                        "the incoming caller has no retained source bytes",
                    ));
                };
                (caller, unit, text)
            }
            CalleeFile::Unaddressed => {
                return unavailable("the incoming caller has no admitted source owner").fail();
            }
        };
        append_incoming_calls(
            &mut acquired,
            &subject,
            (caller, unit, text),
            call.from_ranges,
            report.encoding,
            &mut caller_positions,
        )?;
    }
    deduplicate_occurrences(&mut acquired.occurrences);
    Ok(acquired)
}

fn retain_incoming_file_references(
    acquired: &mut AcquiredSubject,
    subject: &SymbolId,
    call: CallHierarchyIncomingCall,
    encoding: PositionEncoding,
    provider: &rift_protocol::read::Language,
) {
    for range in call.from_ranges {
        acquired.sources.push(QuerySourceRequest::Reference {
            target: subject.clone(),
            uri: call.from.uri.clone(),
            range,
            encoding,
            provider: provider.clone(),
            facet: RelationshipFacet::Calls,
            file_level: true,
        });
    }
}

fn append_incoming_calls<'text>(
    acquired: &mut AcquiredSubject,
    subject: &SymbolId,
    binding: (
        Option<SymbolId>,
        rift_protocol::read::SourceUnitId,
        &'text str,
    ),
    ranges: Vec<Range>,
    encoding: PositionEncoding,
    caller_positions: &mut BTreeMap<
        rift_protocol::read::SourceUnitId,
        rift_lsp::position::LineIndex<'text>,
    >,
) -> Result<(), RiftError> {
    let (caller, unit, text) = binding;
    let positions = caller_positions
        .entry(unit.clone())
        .or_insert_with(|| rift_lsp::position::LineIndex::new(text));
    let sources = ranges
        .into_iter()
        .map(|range| {
            indexed_range(positions, range, encoding).map(|range| SourceUnitSpan {
                unit: unit.clone(),
                range,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(caller) = caller {
        name_package_occurrences(
            &mut acquired.occurrences,
            &mut acquired.relationships,
            &caller,
            subject,
            &sources,
        );
    } else {
        acquired
            .occurrences
            .extend(sources.into_iter().map(|source| QueryOccurrence {
                target: subject.clone(),
                enclosing: None,
                source,
                facet: RelationshipFacet::Calls,
            }));
    }
    Ok(())
}

struct OutgoingReport {
    maximum: usize,
    calls: Vec<CallHierarchyOutgoingCall>,
    encoding: PositionEncoding,
}

async fn outgoing_on_engine(
    slot: &super::EngineSlot,
    target: &ReferenceTarget,
    deadline: Instant,
    maximum: usize,
    requests: ProviderRequests,
) -> Result<OutgoingAnswer<OutgoingReport>, RiftError> {
    let uri = target.uri.clone();
    let utf8 = target.utf8;
    let utf16 = target.utf16;
    slot.request_outgoing(
        target.opening(),
        move |session: &mut EngineSession| {
            let uri = uri.clone();
            let requests = requests.clone();
            Box::pin(async move {
                let encoding = session.capabilities().position_encoding;
                let position = match encoding {
                    PositionEncoding::Utf8 => utf8,
                    PositionEncoding::Utf16 => utf16,
                };
                requests.claim()?;
                let items = session.prepare_call_hierarchy_uri(&uri, position).await?;
                if items.is_empty() {
                    return Ok(None);
                }
                let mut calls = Vec::new();
                for item in items {
                    requests.claim()?;
                    calls.extend(session.outgoing_calls(item).await?);
                    if calls.len() > maximum {
                        return unavailable(
                            "the outgoing answer exceeds the engine response bound",
                        )
                        .fail();
                    }
                }
                Ok(Some(OutgoingReport {
                    maximum,
                    calls,
                    encoding,
                }))
            })
        },
        target.closing(),
        deadline,
    )
    .await
}

#[cfg(test)]
fn map_occurrences(
    reads: &ReadService,
    target: &CoreSymbolId,
    report: ReferenceReport,
    workspace_root: &std::path::Path,
) -> Result<Vec<QueryOccurrence>, RiftError> {
    let (file, symbol) = resolve_graph_symbol(reads.index(), target)?
        .ok_or_else(|| unavailable("the reference target has no selected declaration"))?;
    let range = symbol
        .name_range
        .ok_or_else(|| unavailable("the reference target has no exact declaration name range"))?;
    let excluded = Some((
        crate::read::source_unit_id(file.path()),
        TextRange {
            start: range.start,
            end: range.end,
        },
    ));
    let batch = map_complete_references(
        reads,
        target,
        report,
        workspace_root,
        &CalleeRoots::default(),
        &rift_search::query::QueryFacts::default(),
        (file.syntax().language(), excluded),
    )?;
    Ok(batch.occurrences)
}

fn indexed_range(
    index: &rift_lsp::position::LineIndex<'_>,
    range: Range,
    encoding: PositionEncoding,
) -> Result<TextRange, RiftError> {
    let start = index.byte_offset(encoding, range.start)?;
    let end = index.byte_offset(encoding, range.end)?;
    if start > end {
        return unavailable("an engine range ends before it starts").fail();
    }
    Ok(TextRange {
        start: start as u64,
        end: end as u64,
    })
}

#[cfg(test)]
fn map_calls(
    reads: &ReadService,
    caller: &CoreSymbolId,
    caller_file: &IndexedFile,
    report: OutgoingReport,
    roots: &CalleeRoots,
    workspace_root: &std::path::Path,
) -> Result<AcquiredSubject, RiftError> {
    let source = CallSource {
        text: caller_file.source(),
        unit: crate::read::source_unit_id(caller_file.path()),
        provider: caller_file.syntax().language(),
    };
    map_source_calls(
        reads,
        caller,
        &source,
        report,
        roots,
        workspace_root,
        &rift_search::query::QueryFacts::default(),
    )
}

struct CallSource<'source> {
    text: &'source str,
    unit: rift_protocol::read::SourceUnitId,
    provider: &'source rift_protocol::read::Language,
}

fn map_source_calls(
    reads: &ReadService,
    caller: &CoreSymbolId,
    source: &CallSource<'_>,
    report: OutgoingReport,
    roots: &CalleeRoots,
    workspace_root: &std::path::Path,
    known: &rift_search::query::QueryFacts,
) -> Result<AcquiredSubject, RiftError> {
    let work = report.calls.iter().try_fold(0usize, |count, call| {
        count.checked_add(call.from_ranges.len().max(1))
    });
    if work.is_none_or(|count| count > report.maximum) {
        return unavailable("the outgoing occurrences exceed the engine response bound").fail();
    }
    let trees = [workspace_root, reads.index().root()];
    let mut acquired = AcquiredSubject::default();
    let positions = rift_lsp::position::LineIndex::new(source.text);
    for call in report.calls {
        let named = NamedCallee {
            uri: &call.to.uri,
            name: &call.to.name,
            position: call.to.selection_range.start,
            encoding: report.encoding,
        };
        let sources = call
            .from_ranges
            .into_iter()
            .map(|range| {
                indexed_range(&positions, range, report.encoding).map(|range| SourceUnitSpan {
                    unit: source.unit.clone(),
                    range,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let path = match callee_file(roots, &trees, caller, named)? {
            CalleeFile::Project(path) => path,
            CalleeFile::Package(package) => {
                if let Some(target) = known_package_symbol(&package, known)? {
                    name_package_occurrences(
                        &mut acquired.occurrences,
                        &mut acquired.relationships,
                        &SymbolId(caller.as_str().to_owned()),
                        &target,
                        &sources,
                    );
                    continue;
                }
                acquired
                    .packages
                    .push(package.with_query_sources(sources, source.provider.clone()));
                continue;
            }
            CalleeFile::Unaddressed => {
                return unavailable(format!(
                    "the outgoing target has no admitted source owner: uri {}, served root {}",
                    call.to.uri.as_str(),
                    reads.index().root().display()
                ))
                .fail();
            }
        };
        let target = project_declaration(
            reads,
            &path,
            (call.to.selection_range, report.encoding),
            "query calls",
        )?
        .ok_or_else(|| unavailable("the outgoing target has no selected declaration"))?;
        name_package_occurrences(
            &mut acquired.occurrences,
            &mut acquired.relationships,
            &SymbolId(caller.as_str().to_owned()),
            &target,
            &sources,
        );
    }
    deduplicate_occurrences(&mut acquired.occurrences);
    Ok(acquired)
}

fn known_package_symbol(
    callee: &super::PackageCallee,
    facts: &rift_search::query::QueryFacts,
) -> Result<Option<SymbolId>, RiftError> {
    let Some(origin) = callee.query_origin() else {
        return Ok(None);
    };
    let Some(unit) = callee.unit(&origin) else {
        return Ok(None);
    };
    let Some(source) = facts
        .sources
        .iter()
        .find(|source| source.unit == unit)
        .and_then(|source| source.text.as_deref())
    else {
        return Ok(None);
    };
    let offset = rift_lsp::position::LineIndex::new(source)
        .byte_offset(callee.encoding(), callee.position())? as u64;
    Ok(facts
        .symbols
        .iter()
        .flat_map(|symbol| {
            symbol
                .declarations
                .iter()
                .map(move |declaration| (symbol, declaration))
        })
        .filter(|(_, declaration)| {
            declaration.unit.as_ref() == Some(&unit)
                && declaration.range.start <= offset
                && offset < declaration.range.end
        })
        .min_by_key(|(_, declaration)| declaration.range.end - declaration.range.start)
        .and_then(|(symbol, _)| symbol.symbol.id.clone()))
}

pub(super) fn name_package_occurrences(
    occurrences: &mut Vec<QueryOccurrence>,
    relationships: &mut Vec<Relationship>,
    caller: &SymbolId,
    target: &SymbolId,
    sources: &[SourceUnitSpan],
) {
    if sources.is_empty() {
        relationships.push(super::call_hop(caller, target.clone()).relationship);
    }
    for source in sources {
        let occurrence = QueryOccurrence {
            target: target.clone(),
            enclosing: Some(caller.clone()),
            source: source.clone(),
            facet: RelationshipFacet::Calls,
        };
        relationships.extend(relationship(&occurrence));
        occurrences.push(occurrence);
    }
}

fn deduplicate_occurrences(occurrences: &mut Vec<QueryOccurrence>) {
    occurrences.sort_by(compare_occurrences);
    occurrences.dedup();
}

fn compare_occurrences(left: &QueryOccurrence, right: &QueryOccurrence) -> std::cmp::Ordering {
    (
        &left.source.unit,
        left.source.range.start,
        left.source.range.end,
        &left.target,
        left.facet,
        &left.enclosing,
    )
        .cmp(&(
            &right.source.unit,
            right.source.range.start,
            right.source.range.end,
            &right.target,
            right.facet,
            &right.enclosing,
        ))
}

fn deduplicate_relationships(relationships: &mut Vec<Relationship>) {
    relationships.sort_by(|left, right| {
        let left_source = left
            .occurrence
            .as_ref()
            .map(|source| (&source.unit, source.range.start, source.range.end));
        let right_source = right
            .occurrence
            .as_ref()
            .map(|source| (&source.unit, source.range.start, source.range.end));
        (
            &left.from,
            &left.to,
            &left.kind.0,
            &left.facets,
            left_source,
        )
            .cmp(&(
                &right.from,
                &right.to,
                &right.kind.0,
                &right.facets,
                right_source,
            ))
    });
    relationships.dedup();
}

fn relationship(occurrence: &QueryOccurrence) -> Option<Relationship> {
    Some(Relationship {
        from: occurrence.enclosing.clone()?,
        to: occurrence.target.clone(),
        kind: super::ExactKind(occurrence.facet.as_ref().to_owned()),
        facets: vec![occurrence.facet],
        evidence: Vec::new(),
        occurrence: Some(occurrence.source.clone()),
        derivation: RelationshipDerivation::Resolution,
        confidence: None,
        extensions: Extensions::default(),
    })
}

pub(super) fn retained_occurrence_bytes(occurrences: &Vec<QueryOccurrence>) -> usize {
    occurrences.iter().fold(
        occurrences
            .capacity()
            .saturating_mul(std::mem::size_of::<QueryOccurrence>()),
        |bytes, occurrence| {
            bytes
                .saturating_add(occurrence.target.0.capacity())
                .saturating_add(occurrence.source.unit.0.capacity())
                .saturating_add(
                    occurrence
                        .enclosing
                        .as_ref()
                        .map_or(0, |symbol| symbol.0.capacity()),
                )
        },
    )
}

pub(crate) fn retained_package_bytes(facts: &rift_search::query::QueryFacts) -> usize {
    use get_size2::{GetSize, StandardTracker};
    let mut tracker = StandardTracker::default();
    let mut bytes = facts
        .symbols
        .capacity()
        .saturating_mul(std::mem::size_of::<rift_search::query::QuerySymbol>())
        .saturating_add(
            facts
                .sources
                .capacity()
                .saturating_mul(std::mem::size_of::<rift_search::query::QuerySource>()),
        );
    for symbol in &facts.symbols {
        let (symbol_bytes, next) = symbol.symbol.get_heap_size_with_tracker(tracker);
        let (declarations, next) =
            rift_protocol::retained_vector_material(&symbol.declarations, next);
        tracker = next;
        bytes = bytes
            .saturating_add(symbol_bytes)
            .saturating_add(declarations);
    }
    for source in &facts.sources {
        let (material, next) = source.get_heap_size_with_tracker(tracker);
        tracker = next;
        bytes = bytes.saturating_add(material);
    }
    let (relationships, tracker) =
        rift_protocol::retained_vector_material(&facts.relationships, tracker);
    let (records, tracker) =
        rift_protocol::retained_vector_material(&facts.relationship_records, tracker);
    let (usages, tracker) =
        rift_protocol::retained_vector_material(&facts.occurrence_records, tracker);
    let (providers, tracker) = rift_protocol::retained_vector_material(&facts.providers, tracker);
    let (provider_edges, _) =
        rift_protocol::retained_vector_material(&facts.provider_edges, tracker);
    bytes
        .saturating_add(relationships)
        .saturating_add(records)
        .saturating_add(usages)
        .saturating_add(providers)
        .saturating_add(provider_edges)
        .saturating_add(retained_occurrence_bytes(&facts.occurrences))
}

#[cfg(test)]
mod tests {
    use super::{
        OutgoingReport, map_calls, map_occurrences, relationship, retained_occurrence_bytes,
    };
    use crate::engine_read::ReferenceReport;
    use crate::engine_read::tests::{location, reads, symbol};
    use rift_lsp::capabilities::PositionEncoding;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn retained_package_material_counts_provider_array_capacity() {
        let mut facts = rift_search::query::QueryFacts::default();
        let before = super::retained_package_bytes(&facts);
        facts.providers.reserve_exact(32);
        facts.provider_edges.reserve_exact(16);
        let expected = facts.providers.capacity()
            * std::mem::size_of::<rift_search::query::QueryProviderSnapshot>()
            + facts.provider_edges.capacity()
                * std::mem::size_of::<rift_search::query::QueryProviderEdge>();
        assert_eq!(super::retained_package_bytes(&facts), before + expected);
    }

    #[test]
    fn references_keep_recursive_and_repeated_occurrences() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() { beacon(); beacon(); }\n",
        )?;
        let reads = reads(directory.path())?;
        let target = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let report = ReferenceReport {
            locations: Ok(vec![
                location(directory.path(), "lib.rs", 0, 7, 13),
                location(directory.path(), "lib.rs", 0, 18, 24),
                location(directory.path(), "lib.rs", 0, 28, 34),
                location(directory.path(), "lib.rs", 0, 18, 24),
            ]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let occurrences = map_occurrences(&reads, &target, report, directory.path())?;
        assert_eq!(occurrences.len(), 2);
        assert_eq!(occurrences[0].source.range.start, 18);
        assert_eq!(occurrences[1].source.range.start, 28);
        for occurrence in &occurrences {
            let edge = relationship(occurrence).expect("recursive relationship");
            assert_eq!(edge.from, edge.to);
            assert_eq!(edge.occurrence.as_ref(), Some(&occurrence.source));
        }
        Ok(())
    }

    #[test]
    fn references_keep_file_level_locations_without_declaration_edges() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("lib.py"),
            "def beacon(): pass\nbeacon()\n",
        )?;
        crate::engine_read::tests::python_module_metadata(directory.path(), "['lib']")?;
        let reads = reads(directory.path())?;
        let target = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let report = ReferenceReport {
            locations: Ok(vec![
                location(directory.path(), "lib.py", 0, 4, 10),
                location(directory.path(), "lib.py", 1, 0, 6),
            ]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let occurrences = map_occurrences(&reads, &target, report, directory.path())?;
        assert_eq!(occurrences.len(), 1);
        assert_eq!(occurrences[0].enclosing, None);
        assert_eq!(occurrences[0].source.range.start, 19);
        assert_eq!(relationship(&occurrences[0]), None);
        assert!(retained_occurrence_bytes(&occurrences) > std::mem::size_of_val(&occurrences));
        Ok(())
    }

    #[test]
    fn references_keep_more_than_the_legacy_traversal_bound() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source = format!("def beacon(): pass\n{}", "beacon()\n".repeat(10_001));
        std::fs::write(directory.path().join("lib.py"), source)?;
        crate::engine_read::tests::python_module_metadata(directory.path(), "['lib']")?;
        let reads = reads(directory.path())?;
        let target = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let report = ReferenceReport {
            locations: Ok((1..=10_001)
                .map(|line| location(directory.path(), "lib.py", line, 0, 6))
                .collect()),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let occurrences = map_occurrences(&reads, &target, report, directory.path())?;
        assert_eq!(occurrences.len(), 10_001);
        let batch = super::AcquiredSubject {
            occurrences,
            ..super::AcquiredSubject::default()
        };
        let mut references = crate::EngineReferences::default();
        let maximum = rift_protocol::configuration::WorkspaceConfiguration::default()
            .search
            .query
            .statements;
        super::append_batch(&mut references, batch, usize::try_from(maximum)?, None)?;
        assert_eq!(references.query_occurrences().len(), 10_001);
        assert!(references.is_complete());
        Ok(())
    }

    #[test]
    fn references_convert_utf16_over_crlf_and_preserve_source_bytes() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source = "pub fn beacon() {}\r\nfn caller() { let _ = \"😀\"; beacon(); }\r\n";
        std::fs::write(directory.path().join("lib.rs"), source)?;
        let reads = reads(directory.path())?;
        let target = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let start = source.rfind("beacon").expect("reference offset");
        let index = rift_lsp::position::LineIndex::new(source);
        let start_position = index.position(PositionEncoding::Utf16, start)?;
        let end_position = index.position(PositionEncoding::Utf16, start + 6)?;
        let mut called = location(directory.path(), "lib.rs", 1, 0, 1);
        called.range = lsp_types::Range {
            start: start_position,
            end: end_position,
        };
        let report = ReferenceReport {
            locations: Ok(vec![called]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let occurrences = map_occurrences(&reads, &target, report, directory.path())?;
        assert_eq!(occurrences[0].source.range.start, start as u64);
        assert_eq!(occurrences[0].source.range.end, (start + 6) as u64);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("lib.rs"))?,
            source
        );
        Ok(())
    }

    #[test]
    fn references_refuse_incomplete_answers_and_invalid_ranges() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let target = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let outside = tempfile::tempdir()?;
        for (locations, full) in [
            (
                Ok(vec![location(outside.path(), "foreign.rs", 0, 0, 6)]),
                true,
            ),
            (Ok(Vec::new()), false),
            (Err(10_001), true),
            (
                Ok(vec![location(directory.path(), "lib.rs", 0, 13, 7)]),
                true,
            ),
            (
                Ok(vec![location(directory.path(), "lib.rs", 0, 80, 81)]),
                true,
            ),
        ] {
            let report = ReferenceReport {
                locations,
                encoding: PositionEncoding::Utf16,
                full,
            };
            assert!(map_occurrences(&reads, &target, report, directory.path()).is_err());
        }
        Ok(())
    }

    #[test]
    fn calls_keep_each_caller_range_and_empty_range_edges() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source = "pub fn beacon() {}\nfn caller() { beacon(); beacon(); }\n";
        std::fs::write(directory.path().join("lib.rs"), source)?;
        let reads = reads(directory.path())?;
        let caller = rift_core::SymbolId::new(symbol(&reads, "caller").0)?;
        let file = reads
            .index()
            .file(&rift_core::ProjectPath::new("lib.rs")?)
            .expect("indexed caller source");
        let target = crate::engine_read::tests::item(
            location(directory.path(), "lib.rs", 0, 7, 13),
            "beacon",
        );
        let index = rift_lsp::position::LineIndex::new(source);
        let ranges = source
            .match_indices("beacon")
            .skip(1)
            .map(|(offset, _)| {
                Ok(lsp_types::Range {
                    start: index.position(PositionEncoding::Utf16, offset)?,
                    end: index.position(PositionEncoding::Utf16, offset + 6)?,
                })
            })
            .collect::<Result<Vec<_>, rift_error::RiftError>>()?;
        let report = OutgoingReport {
            maximum: super::TRAVERSAL_NODES_MAX,
            calls: vec![lsp_types::CallHierarchyOutgoingCall {
                to: target.clone(),
                from_ranges: ranges,
            }],
            encoding: PositionEncoding::Utf16,
        };
        let roots = crate::CalleeRoots::default();
        let mapped = map_calls(&reads, &caller, file, report, &roots, directory.path())?;
        assert_eq!(mapped.occurrences.len(), 2);
        assert_eq!(mapped.relationships.len(), 2);
        assert!(mapped.packages.is_empty());
        for occurrence in mapped.occurrences {
            assert_eq!(
                occurrence
                    .enclosing
                    .as_ref()
                    .map(|identity| identity.0.as_str()),
                Some(caller.as_str())
            );
            assert_eq!(occurrence.source.unit.0, "rift://source/project/lib.rs");
            assert_eq!(occurrence.target, symbol(&reads, "beacon"));
        }
        let empty = OutgoingReport {
            maximum: super::TRAVERSAL_NODES_MAX,
            calls: vec![lsp_types::CallHierarchyOutgoingCall {
                to: target,
                from_ranges: Vec::new(),
            }],
            encoding: PositionEncoding::Utf16,
        };
        let mapped = map_calls(&reads, &caller, file, empty, &roots, directory.path())?;
        assert!(mapped.occurrences.is_empty());
        assert_eq!(mapped.relationships.len(), 1);
        assert!(mapped.relationships[0].occurrence.is_none());
        Ok(())
    }
    #[test]
    fn incoming_calls_keep_caller_ranges_and_refuse_invalid_positions() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source =
            "pub fn beacon() {}\r\nfn caller() { let _ = \"😀\"; beacon(); beacon(); }\r\n";
        std::fs::write(directory.path().join("lib.rs"), source)?;
        let reads = reads(directory.path())?;
        let target = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let caller = crate::engine_read::tests::item(
            location(directory.path(), "lib.rs", 1, 3, 9),
            "caller",
        );
        let positions = rift_lsp::position::LineIndex::new(source);
        let ranges = source
            .match_indices("beacon")
            .skip(1)
            .map(|(offset, _)| {
                Ok(lsp_types::Range {
                    start: positions.position(PositionEncoding::Utf16, offset)?,
                    end: positions.position(PositionEncoding::Utf16, offset + 6)?,
                })
            })
            .collect::<Result<Vec<_>, rift_error::RiftError>>()?;
        let report = super::IncomingReport {
            maximum: 2,
            calls: vec![lsp_types::CallHierarchyIncomingCall {
                from: caller.clone(),
                from_ranges: ranges.clone(),
            }],
            encoding: PositionEncoding::Utf16,
        };
        let roots = crate::CalleeRoots::default();
        let facts = rift_search::query::QueryFacts::default();
        let provider = rift_protocol::read::Language::from_identity_segment("rust")?;
        let mapped = super::map_incoming_calls(
            &reads,
            &target,
            report,
            &roots,
            directory.path(),
            &facts,
            &provider,
        )?;
        assert_eq!(mapped.occurrences.len(), 2);
        for occurrence in &mapped.occurrences {
            assert_eq!(occurrence.target, symbol(&reads, "beacon"));
            assert_eq!(occurrence.enclosing, Some(symbol(&reads, "caller")));
            assert_eq!(
                occurrence.facet,
                rift_protocol::read::RelationshipFacet::Calls
            );
            assert_eq!(
                &source[usize::try_from(occurrence.source.range.start)?
                    ..usize::try_from(occurrence.source.range.end)?],
                "beacon"
            );
        }
        let mut module = caller.clone();
        module.kind = lsp_types::SymbolKind::MODULE;
        module.range = lsp_types::Range::default();
        module.selection_range = lsp_types::Range::default();
        let module_calls = super::map_incoming_calls(
            &reads,
            &target,
            super::IncomingReport {
                maximum: 2,
                calls: vec![lsp_types::CallHierarchyIncomingCall {
                    from: module,
                    from_ranges: ranges,
                }],
                encoding: PositionEncoding::Utf16,
            },
            &roots,
            directory.path(),
            &facts,
            &provider,
        )?;
        assert_file_level_calls(&module_calls);
        let report = super::IncomingReport {
            maximum: 1,
            calls: vec![lsp_types::CallHierarchyIncomingCall {
                from: caller,
                from_ranges: vec![lsp_types::Range::new(
                    lsp_types::Position::new(50, 0),
                    lsp_types::Position::new(50, 6),
                )],
            }],
            encoding: PositionEncoding::Utf16,
        };
        assert!(
            super::map_incoming_calls(
                &reads,
                &target,
                report,
                &roots,
                directory.path(),
                &facts,
                &provider
            )
            .is_err()
        );
        Ok(())
    }
    fn assert_file_level_calls(mapped: &super::AcquiredSubject) {
        assert_eq!(mapped.occurrences.len(), 2);
        assert!(
            mapped
                .occurrences
                .iter()
                .all(|occurrence| occurrence.enclosing.is_none()
                    && occurrence.facet == rift_protocol::read::RelationshipFacet::Calls)
        );
        assert!(mapped.relationships.is_empty());
    }

    #[test]
    fn raw_records_keep_distinct_identities_and_refuse_payload_conflicts() -> TestResult {
        use rift_protocol::relationship_read::{RelationshipRecord, RelationshipRecordId};
        let source = rift_protocol::read::SourceUnitSpan {
            unit: rift_protocol::read::SourceUnitId("rift://source/project/lib.rs".to_owned()),
            range: rift_protocol::read::TextRange { start: 1, end: 7 },
        };
        let usage = rift_protocol::query::QueryOccurrence {
            target: rift_protocol::read::SymbolId(
                "rift://symbol/local/rust/beacon/target".to_owned(),
            ),
            enclosing: Some(rift_protocol::read::SymbolId(
                "rift://symbol/local/rust/beacon/caller".to_owned(),
            )),
            source,
            facet: rift_protocol::read::RelationshipFacet::Calls,
        };
        let relationship = super::relationship(&usage).expect("physical call edge");
        let first = RelationshipRecord {
            id: RelationshipRecordId::parse(&"1".repeat(64))?,
            relationship: relationship.clone(),
        };
        let second = RelationshipRecord {
            id: RelationshipRecordId::parse(&"2".repeat(64))?,
            relationship,
        };
        let mut facts = rift_search::query::QueryFacts::default();
        super::merge_package_facts(
            &mut facts,
            rift_search::query::QueryFacts {
                relationship_records: vec![first.clone(), second],
                ..rift_search::query::QueryFacts::default()
            },
        )?;
        super::merge_package_facts(
            &mut facts,
            rift_search::query::QueryFacts {
                relationship_records: vec![first.clone()],
                ..rift_search::query::QueryFacts::default()
            },
        )?;
        assert_eq!(facts.relationship_records.len(), 2);
        assert!(facts.relationships.is_empty());
        let mut conflict = first;
        conflict.relationship.kind = rift_protocol::read::ExactKind("references".to_owned());
        assert!(
            super::merge_package_facts(
                &mut facts,
                rift_search::query::QueryFacts {
                    relationship_records: vec![conflict],
                    ..rift_search::query::QueryFacts::default()
                }
            )
            .is_err()
        );
        let references = crate::EngineReferences {
            query_package_facts: facts,
            ..crate::EngineReferences::default()
        };
        assert_eq!(references.query_relationships().len(), 2);
        assert_eq!(references.query_local_relationships().count(), 0);
        assert_eq!(
            references.query_package_facts().relationship_records.len(),
            2
        );
        assert!(
            super::retained_package_bytes(references.query_package_facts())
                > std::mem::size_of::<rift_search::query::QueryFacts>()
        );
        Ok(())
    }

    #[test]
    fn provider_request_counter_is_shared_and_refuses_before_overflow() -> TestResult {
        let requests = super::ProviderRequests::new(2);
        requests.claim()?;
        let same = requests.clone();
        same.claim()?;
        assert!(requests.claim().is_err());
        assert_eq!(requests.used.load(std::sync::atomic::Ordering::Relaxed), 2);
        Ok(())
    }

    #[test]
    fn original_usage_records_keep_identity_locations_and_measured_material() -> TestResult {
        use rift_protocol::relationship_read::{RelationshipOccurrence, RelationshipRecordId};
        let occurrence = serde_json::from_value(serde_json::json!({
            "target":"rift://symbol/cargo/crates.io/demo@1.0.0/rust/target",
            "source":{"unit":"rift://source/cargo/crates.io/demo@1.0.0/src/lib.rs","range":{"start":0,"end":3}},
            "facet":"references"
        }))?;
        let first = RelationshipOccurrence {
            id: RelationshipRecordId::parse(&"a".repeat(64))?,
            relationship: None,
            occurrence,
        };
        let mut second = first.clone();
        second.id = RelationshipRecordId::parse(&"b".repeat(64))?;
        let mut another_location = first.clone();
        another_location.occurrence.source.range.start = 1;
        let mut facts = rift_search::query::QueryFacts::default();
        super::merge_package_facts(
            &mut facts,
            rift_search::query::QueryFacts {
                occurrence_records: vec![first.clone(), second, another_location],
                ..rift_search::query::QueryFacts::default()
            },
        )?;
        super::merge_package_facts(
            &mut facts,
            rift_search::query::QueryFacts {
                occurrence_records: vec![first.clone()],
                ..rift_search::query::QueryFacts::default()
            },
        )?;
        assert!(facts.occurrences.is_empty());
        assert_eq!(facts.occurrence_records.len(), 3);
        assert!(super::retained_package_bytes(&facts) > std::mem::size_of_val(&facts));
        let mut changed = first;
        changed.relationship = Some(RelationshipRecordId::parse(&"c".repeat(64))?);
        assert!(
            super::merge_package_facts(
                &mut facts,
                rift_search::query::QueryFacts {
                    occurrence_records: vec![changed],
                    ..rift_search::query::QueryFacts::default()
                }
            )
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn complete_relationship_cache_keeps_direction_and_all_facet_selection() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let seed = symbol(&reads, "beacon");
        let engines = crate::EnginePool::new(
            directory.path(),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
        );
        let configuration = rift_protocol::configuration::WorkspaceConfiguration::default();
        let roots = crate::CalleeRoots::default();
        let mut query = super::EngineQuery::new(
            &reads,
            &engines,
            &configuration,
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
            &roots,
        );
        let calls = [rift_protocol::read::RelationshipFacet::Calls];
        let incoming = rift_protocol::read::TraversalDirection::Incoming;
        let outgoing = rift_protocol::read::TraversalDirection::Outgoing;
        query.admit_relationship_records(&seed, outgoing, &calls, Vec::new(), Vec::new())?;
        assert!(query.relationships_complete(&seed, outgoing, &calls));
        assert!(!query.relationships_complete(&seed, incoming, &calls));
        assert!(!query.relationships_complete(
            &seed,
            outgoing,
            &[rift_protocol::read::RelationshipFacet::References]
        ));
        query.admit_relationship_records(&seed, incoming, &[], Vec::new(), Vec::new())?;
        assert!(query.relationships_complete(&seed, incoming, &calls));
        assert!(query.relationships_complete(
            &seed,
            incoming,
            &[rift_protocol::read::RelationshipFacet::References]
        ));
        assert!(query.references().is_complete());
        Ok(())
    }
    #[test]
    fn source_digest_merge_keeps_exact_physical_bytes_and_full_hash() -> TestResult {
        for (original, changed) in [
            ("pub fn f() {}\r\n", "pub fn f() {}\n"),
            ("pub fn f() {}", "pub fn f() {}\n"),
            ("é", "e\u{301}"),
        ] {
            let unit =
                rift_protocol::read::SourceUnitId("rift://source/project/bytes.rs".to_owned());
            let source = |text: &str| rift_search::query::QueryFacts {
                sources: vec![rift_search::query::QuerySource {
                    unit: unit.clone(),
                    digest: None,
                    text: Some(text.to_owned()),
                }],
                ..rift_search::query::QueryFacts::default()
            };
            let mut held = rift_search::query::QueryFacts::default();
            super::merge_package_facts(&mut held, source(original))?;
            let mut metadata = held.clone();
            metadata.sources[0].text = None;
            super::merge_package_facts(&mut held, metadata)?;
            assert_eq!(
                held.sources[0]
                    .text
                    .as_deref()
                    .expect("captured bytes")
                    .as_bytes(),
                original.as_bytes()
            );
            let mut altered = source(changed);
            altered.sources[0].digest = held.sources[0].digest.clone();
            assert!(super::merge_package_facts(&mut held, altered).is_err());
        }
        Ok(())
    }

    #[test]
    fn source_projection_merges_missing_bytes_without_overwriting_captured_bytes() -> TestResult {
        let unit = rift_protocol::read::SourceUnitId("rift://source/project/lib.rs".to_owned());
        let source = |text: Option<&str>| rift_search::query::QueryFacts {
            sources: vec![rift_search::query::QuerySource {
                unit: unit.clone(),
                digest: None,
                text: text.map(str::to_owned),
            }],
            ..rift_search::query::QueryFacts::default()
        };
        let mut facts = source(None);
        super::merge_package_facts(&mut facts, source(Some("original")))?;
        assert_eq!(facts.sources[0].text.as_deref(), Some("original"));
        super::merge_package_facts(&mut facts, source(None))?;
        assert_eq!(facts.sources[0].text.as_deref(), Some("original"));
        assert_eq!(facts.sources.len(), 1);
        assert!(super::merge_package_facts(&mut facts, source(Some("changed"))).is_err());
        Ok(())
    }
}
