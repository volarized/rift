//! Physical dependency targets retained by the request's original normalized owner.

use super::{
    Arc, BTreeMap, BTreeSet, EngineQuery, PositionEncoding, QueryPackageTarget, QuerySourceRequest,
    Range, ReferenceTarget, RelationshipFacet, RiftError, SourceUnitSpan, SymbolId, TextRange,
    TraversalDirection, deduplicate_provider_edges, errors, incoming_on_engine, indexed_range,
    outgoing_on_engine, outgoing_report, project_declaration, references_on_engine_with_bound,
    symbol_id, unavailable,
};
use rift_search::query::{QueryProviderEdge, QueryProviderTarget};

#[derive(Debug, Clone)]
pub(crate) struct PreparedEdge {
    from: Option<QueryProviderTarget>,
    to: QueryProviderTarget,
    facet: RelationshipFacet,
    occurrence: Option<SourceUnitSpan>,
}

impl get_size2::GetSize for PreparedEdge {
    fn get_size(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(self.get_heap_size())
    }

    fn get_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (heap, tracker) = self.get_heap_size_with_tracker(tracker);
        (std::mem::size_of::<Self>().saturating_add(heap), tracker)
    }

    fn get_heap_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (from, tracker) = self.from.get_heap_size_with_tracker(tracker);
        let (to, tracker) = self.to.get_heap_size_with_tracker(tracker);
        let (occurrence, tracker) = self.occurrence.get_heap_size_with_tracker(tracker);
        (from.saturating_add(to).saturating_add(occurrence), tracker)
    }
}

struct PhysicalAcquisition<'source> {
    subject: &'source QueryProviderTarget,
    provider: &'source rift_protocol::read::Language,
    declaration: &'source SourceUnitSpan,
    target: &'source ReferenceTarget,
    positions: &'source rift_lsp::position::LineIndex<'source>,
    slot: &'source super::super::EngineSlot,
    maximum: usize,
    direction: TraversalDirection,
    facet: RelationshipFacet,
}

impl EngineQuery<'_> {
    fn primary_provider_count(&self) -> usize {
        self.query_facts.map_or(0, |facts| facts.providers.len())
    }

    /// Selects an endpoint in the original primary or admitted provider directory.
    #[must_use]
    pub fn provider_endpoint(
        &self,
        target: &QueryProviderTarget,
    ) -> Option<rift_search::query::QueryProviderEndpoint> {
        if let Some(endpoint) = self
            .query_facts
            .and_then(|facts| facts.provider_endpoint(target))
        {
            return Some(endpoint);
        }
        let mut endpoint = self
            .references
            .query_package_facts
            .provider_endpoint(target)?;
        if let Some(index) = &mut endpoint.provider {
            *index = index.checked_add(self.primary_provider_count())?;
        }
        Some(endpoint)
    }

    /// Restores an endpoint under its original captured provider context.
    #[must_use]
    pub fn provider_target(
        &self,
        endpoint: &rift_search::query::QueryProviderEndpoint,
    ) -> Option<QueryProviderTarget> {
        let count = self.primary_provider_count();
        if endpoint.provider.is_some_and(|index| index < count) {
            let index = endpoint.provider?;
            return Some(QueryProviderTarget {
                context: Some(Arc::clone(self.primary_contexts.get(index)?)),
                target: endpoint.target.clone(),
            });
        }
        let mut endpoint = endpoint.clone();
        if let Some(index) = &mut endpoint.provider {
            *index = index.checked_sub(count)?;
        }
        let Some(index) = endpoint.provider else {
            return self
                .references
                .query_package_facts
                .provider_target(&endpoint);
        };
        let snapshot = self.references.query_package_facts.providers.get(index)?;
        let context = self
            .provider_targets
            .iter()
            .filter_map(|held| held.provider_names.first())
            .filter_map(|(target, _)| target.context.as_ref())
            .find(|context| context.as_ref() == &snapshot.context)
            .cloned()?;
        Some(QueryProviderTarget {
            context: Some(context),
            target: endpoint.target,
        })
    }

    fn physical_reference(
        &self,
        subject: &QueryProviderTarget,
    ) -> Result<
        (
            rift_protocol::read::Language,
            SourceUnitSpan,
            ReferenceTarget,
        ),
        RiftError,
    > {
        if let Some(reference) = self.admitted_physical_reference(subject)? {
            return Ok(reference);
        }
        let facts = self.query_facts.ok_or_else(|| {
            unavailable("the physical subject has no captured declaration name position")
        })?;
        let (snapshot, record) = facts.provider_record(subject).ok_or_else(|| {
            unavailable("the physical subject has no original provider association")
        })?;
        let selection = self
            .provider_selection
            .as_ref()
            .ok_or_else(|| unavailable("the physical subject has no original serving selection"))?;
        if &snapshot.context.selection != selection.as_ref() {
            return unavailable("the physical subject differs from the selected publication")
                .fail();
        }
        let current = self.reads.index().symbol_view()?;
        if selection.fingerprint.as_str()
            != data_encoding::HEXLOWER.encode(current.fingerprint().as_bytes())
        {
            return unavailable("the physical subject source publication is no longer selected")
                .fail();
        }
        let selected = match self.view_input {
            Some(input) => input.provider_view(&snapshot.context)?.ok_or_else(|| {
                unavailable("the physical subject has no original provider association")
            })?,
            None => current.as_ref(),
        };
        validate_original_record(selected, record)?;
        for declaration in &record.declarations {
            let (owner, path) = rift_protocol::identity::parse_source_unit_identity(
                &declaration.binding.unit().to_string(),
            )?;
            if owner
                .as_ref()
                .is_some_and(|owner| owner != &snapshot.context.owner)
            {
                continue;
            }
            let path = rift_core::ProjectPath::new(path)?;
            let Some(file) = selected.file(&path) else {
                continue;
            };
            let Some(symbol) = file.syntax().symbols().iter().find(|symbol| {
                symbol.item_range.start == declaration.binding.range().start()
                    && symbol.item_range.end == declaration.binding.range().end()
            }) else {
                continue;
            };
            let Some(name) = symbol.name_range else {
                continue;
            };
            let provider = file.syntax().language().clone();
            let slot = self
                .engines
                .engine_for(&provider)
                .ok_or_else(|| unavailable("the captured local provider is unavailable"))?;
            let target = ReferenceTarget::new(
                slot.workspace_root(),
                self.reads.index().root(),
                file,
                symbol,
            )?;
            return Ok((
                provider,
                SourceUnitSpan {
                    unit: rift_protocol::read::SourceUnitId(declaration.binding.unit().to_string()),
                    range: TextRange {
                        start: name.start,
                        end: name.end,
                    },
                },
                target,
            ));
        }
        let name = facts
            .symbols
            .get(record.symbol_index)
            .map_or("", |symbol| symbol.symbol.name.as_str());
        unavailable(format!(
            "the physical subject {name:?} has no captured declaration name position"
        ))
        .fail()
    }

    fn admitted_physical_reference(
        &self,
        subject: &QueryProviderTarget,
    ) -> Result<
        Option<(
            rift_protocol::read::Language,
            SourceUnitSpan,
            ReferenceTarget,
        )>,
        RiftError,
    > {
        if let Some((held, declaration)) = self.provider_targets.iter().find_map(|held| {
            held.provider_names
                .iter()
                .find(|(target, _)| target == subject)
                .map(|(_, declaration)| (held, declaration))
        }) {
            let source = QueryPackageTarget::source_for(
                &self.references.query_package_facts,
                &declaration.source.unit,
            )?;
            let positions = rift_lsp::position::LineIndex::new(source);
            let offset = usize::try_from(declaration.source.range.start).map_err(|_| {
                unavailable("the physical declaration position cannot be represented")
            })?;
            return Ok(Some((
                held.provider.clone(),
                declaration.source.clone(),
                ReferenceTarget {
                    language: held.provider.name.clone(),
                    source: source.to_owned(),
                    utf8: positions.position(PositionEncoding::Utf8, offset)?,
                    utf16: positions.position(PositionEncoding::Utf16, offset)?,
                    uri: declaration.uri.clone(),
                    canonical_uri: declaration.uri.clone(),
                },
            )));
        }
        Ok(None)
    }

    fn local_provider_at(
        &self,
        path: &rift_core::ProjectPath,
        selected: &TextRange,
        name: bool,
    ) -> Result<Option<QueryProviderTarget>, RiftError> {
        let Some(facts) = self.query_facts else {
            return Ok(None);
        };
        let Some(selection) = &self.provider_selection else {
            return Ok(None);
        };
        let file = match self.view_input {
            Some(input) => input.indices().find_map(|view| view.file(path)),
            None => self.reads.index().file(path),
        }
        .ok_or_else(|| unavailable("the engine target has no selected source bytes"))?;
        let unit = crate::read::source_unit_id(path);
        Ok(facts
            .providers
            .iter()
            .enumerate()
            .filter(|(_, snapshot)| &snapshot.context.selection == selection.as_ref())
            .flat_map(|(provider, snapshot)| {
                snapshot.records.iter().flat_map(move |record| {
                    record
                        .declarations
                        .iter()
                        .map(move |association| (provider, record, association))
                })
            })
            .filter(|(_, _, association)| {
                let binding = &association.binding;
                binding.unit().to_string() == unit.as_str()
                    && binding.range().start() <= selected.start
                    && selected.end <= binding.range().end()
                    && (!name
                        || file.syntax().symbols().iter().any(|symbol| {
                            symbol.item_range.start == binding.range().start()
                                && symbol.item_range.end == binding.range().end()
                                && symbol.name_range.as_ref().is_some_and(|range| {
                                    range.start <= selected.start && selected.start < range.end
                                })
                        }))
            })
            .min_by_key(|(_, _, association)| {
                association.binding.range().end() - association.binding.range().start()
            })
            .and_then(|(provider, record, _)| {
                let context = self.primary_contexts.get(provider)?;
                Some(QueryProviderTarget {
                    context: Some(Arc::clone(context)),
                    target: record.target()?,
                })
            }))
    }

    /// Acquires supplied canonical or captured physical subjects through shared engine operations.
    ///
    /// # Errors
    /// Refuses unsupported facets, absent captured bindings, incomplete exchanges or exhausted bounds.
    pub async fn resolve_targets(
        &mut self,
        subjects: &[QueryProviderTarget],
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
                .capability("relationship acquisition for the selected direction and facets")
                .fail();
        }
        for requested in subjects {
            let scoped = self.scoped_canonical_target(requested)?;
            let subject = scoped.as_ref().unwrap_or(requested);
            if let Some(identity) = subject.canonical_symbol() {
                self.resolve_facets(&[identity], direction, facets).await?;
                continue;
            }
            for facet in facets {
                let endpoint = self.provider_endpoint(subject).ok_or_else(|| {
                    unavailable("the physical subject has no original provider context")
                })?;
                let key = (endpoint, *facet, direction == TraversalDirection::Outgoing);
                if self.resolved_providers.contains(&key) {
                    continue;
                }
                self.resolve_physical(subject, direction, *facet).await?;
                self.resolved_providers.insert(key);
            }
        }
        Ok(())
    }

    fn scoped_canonical_target(
        &self,
        subject: &QueryProviderTarget,
    ) -> Result<Option<QueryProviderTarget>, RiftError> {
        let Some(input) = self.view_input else {
            return Ok(None);
        };
        if input.indices().nth(1).is_none() {
            return Ok(None);
        }
        let Some(identity) = subject.canonical_symbol() else {
            return Ok(None);
        };
        let Some(facts) = self.query_facts else {
            return Ok(None);
        };
        for (provider, snapshot) in facts.providers.iter().enumerate() {
            if snapshot.records.iter().any(|record| {
                record
                    .identity
                    .as_ref()
                    .is_some_and(|held| held.as_str() == identity.as_str())
            }) {
                let endpoint = rift_search::query::QueryProviderEndpoint {
                    provider: Some(provider),
                    target: subject.target.clone(),
                };
                return self.provider_target(&endpoint).map(Some).ok_or_else(|| {
                    unavailable("the physical subject has no original provider context")
                });
            }
        }
        Ok(None)
    }

    async fn resolve_physical(
        &mut self,
        subject: &QueryProviderTarget,
        direction: TraversalDirection,
        facet: RelationshipFacet,
    ) -> Result<(), RiftError> {
        let (provider, declaration, target) = self.physical_reference(subject)?;
        let slot = self
            .engines
            .engine_for(&provider)
            .ok_or_else(|| unavailable("the admitted dependency provider is unavailable"))?;
        let positions = rift_lsp::position::LineIndex::new(&target.source);
        let maximum = usize::try_from(self.configuration.search.query.statements)
            .map_err(|_| unavailable("the accepted statement bound cannot be represented"))?;
        let mut pending = Vec::new();
        let mut edges = Vec::new();
        let input = PhysicalAcquisition {
            subject,
            provider: &provider,
            declaration: &declaration,
            target: &target,
            positions: &positions,
            slot,
            maximum,
            direction,
            facet,
        };
        let unconfirmed = match (direction, facet) {
            (TraversalDirection::Outgoing, RelationshipFacet::Calls) => {
                self.physical_outgoing_calls(&input, &mut pending, &mut edges)
                    .await?
            }
            (TraversalDirection::Incoming, RelationshipFacet::Calls) => {
                self.physical_incoming_calls(&input, &mut pending, &mut edges)
                    .await?
            }
            (TraversalDirection::Incoming, RelationshipFacet::References) => {
                self.physical_references(&input, &mut pending, &mut edges)
                    .await?
            }
            _ => {
                return errors::server::read_unsupported()
                    .capability("relationship acquisition for the selected direction and facet")
                    .fail();
            }
        };
        if self
            .references
            .query_source_requests
            .len()
            .checked_add(pending.len())
            .is_none_or(|count| count > maximum)
        {
            return unavailable(
                "the pending dependency sources exceed the accepted statement bound",
            )
            .fail();
        }
        self.references.query_source_requests.extend(pending);
        self.admit_prepared_edges(edges)?;
        if unconfirmed {
            self.references.unconfirmed.insert(target.language);
        }
        Ok(())
    }

    async fn physical_outgoing_calls(
        &mut self,
        input: &PhysicalAcquisition<'_>,
        pending: &mut Vec<QuerySourceRequest>,
        edges: &mut Vec<PreparedEdge>,
    ) -> Result<bool, RiftError> {
        let (report, unconfirmed) = outgoing_report(
            Box::pin(outgoing_on_engine(
                input.slot,
                input.target,
                self.deadline,
                input.maximum,
                self.provider_requests.clone(),
            ))
            .await?,
        )?;
        let count = report.calls.iter().try_fold(0usize, |count, call| {
            count.checked_add(call.from_ranges.len().max(1))
        });
        if count.is_none_or(|count| count > input.maximum) {
            return unavailable("the outgoing occurrences exceed the engine response bound").fail();
        }
        for call in report.calls {
            let sources = call
                .from_ranges
                .into_iter()
                .map(|range| {
                    indexed_range(input.positions, range, report.encoding).map(|range| {
                        SourceUnitSpan {
                            unit: input.declaration.unit.clone(),
                            range,
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            self.map_provider_location(
                QuerySourceRequest::Provider {
                    subject: input.subject.clone(),
                    direction: input.direction,
                    facet: input.facet,
                    provider: input.provider.clone(),
                    uri: call.to.uri,
                    range: call.to.selection_range,
                    encoding: report.encoding,
                    name: Some(call.to.name),
                    ranges: Vec::new(),
                    sources,
                    file_level: false,
                },
                pending,
                edges,
            )?;
        }
        Ok(unconfirmed)
    }

    async fn physical_incoming_calls(
        &mut self,
        input: &PhysicalAcquisition<'_>,
        pending: &mut Vec<QuerySourceRequest>,
        edges: &mut Vec<PreparedEdge>,
    ) -> Result<bool, RiftError> {
        let (report, unconfirmed) = outgoing_report(
            Box::pin(incoming_on_engine(
                input.slot,
                input.target,
                self.deadline,
                input.maximum,
                self.provider_requests.clone(),
            ))
            .await?,
        )?;
        let count = report.calls.iter().try_fold(0usize, |count, call| {
            count.checked_add(call.from_ranges.len().max(1))
        });
        if count.is_none_or(|count| count > input.maximum) {
            return unavailable("the incoming call occurrences exceed the engine response bound")
                .fail();
        }
        for call in report.calls {
            let file_level = call.from.kind == lsp_types::SymbolKind::MODULE
                && call.from.range == Range::default()
                && call.from.selection_range == Range::default();
            self.map_provider_location(
                QuerySourceRequest::Provider {
                    subject: input.subject.clone(),
                    direction: input.direction,
                    facet: input.facet,
                    provider: input.provider.clone(),
                    uri: call.from.uri,
                    range: call.from.selection_range,
                    encoding: report.encoding,
                    name: (!file_level).then_some(call.from.name),
                    ranges: call.from_ranges,
                    sources: Vec::new(),
                    file_level,
                },
                pending,
                edges,
            )?;
        }
        Ok(unconfirmed)
    }

    async fn physical_references(
        &mut self,
        input: &PhysicalAcquisition<'_>,
        pending: &mut Vec<QuerySourceRequest>,
        edges: &mut Vec<PreparedEdge>,
    ) -> Result<bool, RiftError> {
        let (report, unconfirmed) = Box::pin(references_on_engine_with_bound(
            input.slot,
            input.target,
            self.deadline,
            input.maximum,
            Some(self.provider_requests.clone()),
        ))
        .await?;
        if !report.full {
            return unavailable("the reference answer did not establish the declaration").fail();
        }
        for location in report
            .locations
            .map_err(|_| unavailable("the reference answer exceeds the engine response bound"))?
        {
            if location.uri == input.target.uri
                && indexed_range(input.positions, location.range, report.encoding)?
                    == input.declaration.range
            {
                continue;
            }
            self.map_provider_location(
                QuerySourceRequest::Provider {
                    subject: input.subject.clone(),
                    direction: input.direction,
                    facet: input.facet,
                    provider: input.provider.clone(),
                    uri: location.uri,
                    range: location.range,
                    encoding: report.encoding,
                    name: None,
                    ranges: vec![location.range],
                    sources: Vec::new(),
                    file_level: false,
                },
                pending,
                edges,
            )?;
        }
        Ok(unconfirmed)
    }

    fn map_provider_location(
        &self,
        request: QuerySourceRequest,
        pending: &mut Vec<QuerySourceRequest>,
        edges: &mut Vec<PreparedEdge>,
    ) -> Result<(), RiftError> {
        let QuerySourceRequest::Provider {
            subject,
            direction,
            facet,
            uri,
            range,
            encoding,
            name,
            ranges,
            sources,
            file_level,
            ..
        } = &request
        else {
            return unavailable("a physical source request requires its original provider subject")
                .fail();
        };
        if !uri
            .scheme()
            .is_some_and(|scheme| scheme.as_str().eq_ignore_ascii_case("file"))
        {
            pending.push(request);
            return Ok(());
        }
        let trees = self.source_trees();
        let trees = trees
            .iter()
            .map(std::path::PathBuf::as_path)
            .collect::<Vec<_>>();
        let path = match self.roots.address(&trees, uri)? {
            Some(rift_lsp::uri::EngineAddress::Project(path)) => path,
            Some(rift_lsp::uri::EngineAddress::Package(_)) => {
                pending.push(request);
                return Ok(());
            }
            None => return unavailable("the engine target has no admitted source owner").fail(),
        };
        let file = match self.view_input {
            Some(input) => input.indices().find_map(|view| view.file(&path)),
            None => self.reads.index().file(&path),
        }
        .ok_or_else(|| unavailable("the engine target has no selected source bytes"))?;
        let unit = crate::read::source_unit_id(file.path());
        let positions = rift_lsp::position::LineIndex::new(file.source());
        let selection = indexed_range(&positions, *range, *encoding)?;
        let selected = if *file_level {
            None
        } else {
            self.provider_location_target(
                file,
                &path,
                &selection,
                (*range, *encoding),
                name.is_some(),
            )?
        };
        match direction {
            TraversalDirection::Outgoing => {
                let selected = selected.ok_or_else(|| {
                    unavailable("the outgoing target has no selected declaration")
                })?;
                append_outgoing_edges(edges, subject, selected, *facet, sources);
            }
            TraversalDirection::Incoming => {
                if ranges.is_empty() {
                    if selected.is_some() {
                        edges.push(PreparedEdge {
                            from: selected,
                            to: subject.clone(),
                            facet: *facet,
                            occurrence: None,
                        });
                    }
                } else {
                    for range in ranges {
                        edges.push(PreparedEdge {
                            from: selected.clone(),
                            to: subject.clone(),
                            facet: *facet,
                            occurrence: Some(SourceUnitSpan {
                                unit: unit.clone(),
                                range: indexed_range(&positions, *range, *encoding)?,
                            }),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    fn provider_location_target(
        &self,
        file: &rift_index::IndexedFile,
        path: &rift_core::ProjectPath,
        selection: &TextRange,
        request: (Range, PositionEncoding),
        named: bool,
    ) -> Result<Option<QueryProviderTarget>, RiftError> {
        let canonical = if let Some(input) = self.view_input {
            file.enclosing_symbol(selection.start, selection.end)
                .and_then(|symbol| {
                    input
                        .indices()
                        .find_map(|view| view.symbol_identity(path, symbol.range))
                })
                .map(|identity| SymbolId(identity.as_str().to_owned()))
        } else if named {
            project_declaration(self.reads, path, request, "query calls")?
        } else {
            file.enclosing_symbol(selection.start, selection.end)
                .map(|symbol| symbol_id(self.reads.index(), crate::search::declared(file, symbol)))
                .transpose()?
                .flatten()
        }
        .as_ref()
        .map(QueryProviderTarget::canonical)
        .transpose()?;
        if self.view_input.is_some() {
            Ok(self
                .local_provider_at(path, selection, named)?
                .or(canonical))
        } else {
            Ok(canonical.or(self.local_provider_at(path, selection, named)?))
        }
    }

    /// Returns reached subjects using original scoped endpoint selectors.
    ///
    /// # Errors
    /// Refuses a relationship whose canonical endpoint is invalid.
    pub fn provider_neighbors(
        &self,
        subject: &rift_search::query::QueryProviderEndpoint,
        direction: TraversalDirection,
        facets: &[RelationshipFacet],
    ) -> Result<BTreeSet<rift_search::query::QueryProviderEndpoint>, RiftError> {
        let mut result = BTreeSet::new();
        let mut follow = |from: Option<&rift_search::query::QueryProviderEndpoint>,
                          to: &rift_search::query::QueryProviderEndpoint,
                          facet| {
            if !facets.is_empty() && !facets.contains(&facet) {
                return;
            }
            match direction {
                TraversalDirection::Outgoing if from == Some(subject) => {
                    result.insert(to.clone());
                }
                TraversalDirection::Incoming if to == subject => {
                    if let Some(from) = from {
                        result.insert(from.clone());
                    }
                }
                _ => {}
            }
        };
        for edge in &self.references.query_provider_edges {
            let from = edge
                .from
                .as_ref()
                .map(|target| {
                    self.provider_endpoint(target).ok_or_else(|| {
                        unavailable("the captured caller has no original provider context")
                    })
                })
                .transpose()?;
            let to = self.provider_endpoint(&edge.to).ok_or_else(|| {
                unavailable("the captured target has no original provider context")
            })?;
            follow(from.as_ref(), &to, edge.facet);
        }
        for edge in self.references.query_relationships() {
            let from = self
                .provider_endpoint(&QueryProviderTarget::canonical(&edge.from)?)
                .ok_or_else(|| unavailable("the relationship caller has no canonical identity"))?;
            let to = self
                .provider_endpoint(&QueryProviderTarget::canonical(&edge.to)?)
                .ok_or_else(|| unavailable("the relationship target has no canonical identity"))?;
            for facet in edge.facets {
                follow(Some(&from), &to, facet);
            }
        }
        for (facts, offset) in
            self.query_facts
                .into_iter()
                .map(|facts| (facts, 0))
                .chain(std::iter::once((
                    &self.references.query_package_facts,
                    self.primary_provider_count(),
                )))
        {
            for edge in &facts.provider_edges {
                let mut from = edge.from.clone();
                let mut to = edge.to.clone();
                for endpoint in from.iter_mut().chain(std::iter::once(&mut to)) {
                    if let Some(provider) = &mut endpoint.provider {
                        *provider = provider.checked_add(offset).ok_or_else(|| {
                            unavailable("the provider directory exceeds the platform bound")
                        })?;
                    }
                }
                follow(from.as_ref(), &to, edge.facet);
            }
            for (provider, snapshot) in facts.providers.iter().enumerate() {
                let provider = provider.checked_add(offset).ok_or_else(|| {
                    unavailable("the provider directory exceeds the platform bound")
                })?;
                let endpoint = |target| rift_search::query::QueryProviderEndpoint {
                    provider: Some(provider),
                    target,
                };
                let source = |key: &rift_core::ContributionKey| {
                    snapshot
                        .records
                        .iter()
                        .find(|record| record.contributions.contains(key))
                        .and_then(rift_search::query::QueryProviderRecord::target)
                        .map(endpoint)
                };
                for reference in &snapshot.references {
                    let from = source(&reference.source);
                    for target in &reference.targets {
                        follow(from.as_ref(), &endpoint(target.clone()), reference.facet);
                    }
                }
                for relationship in &snapshot.relationships {
                    let from = source(&relationship.source);
                    follow(
                        from.as_ref(),
                        &endpoint(relationship.target.clone()),
                        relationship.facet,
                    );
                }
            }
        }
        Ok(result)
    }

    pub(super) fn name_physical_package_sources(
        &mut self,
        target: &QueryPackageTarget,
        facts: &rift_search::query::QueryFacts,
    ) -> Result<Vec<PreparedEdge>, RiftError> {
        let captured_uri = match &target.request {
            QuerySourceRequest::Call(callee)
            | QuerySourceRequest::IncomingCall { caller: callee, .. } => callee.query_uri(),
            QuerySourceRequest::Reference { uri, .. }
            | QuerySourceRequest::Provider { uri, .. } => Some(uri),
        }
        .ok_or_else(|| unavailable("the captured dependency has no original URI"))?;
        let mut prepared = PreparedSources {
            target,
            facts,
            captured_uri,
            edges: Vec::new(),
            positions: BTreeMap::new(),
        };
        let mut waiting = Vec::new();
        for pending in std::mem::take(&mut self.references.package_callees) {
            if !prepared.package_callee(&pending)? {
                waiting.push(pending);
            }
        }
        self.references.package_callees = waiting;
        let mut waiting = Vec::new();
        for request in std::mem::take(&mut self.references.query_source_requests) {
            if !prepared.request(&request)? {
                waiting.push(request);
            }
        }
        self.references.query_source_requests = waiting;
        Ok(prepared.edges)
    }

    pub(super) fn admit_prepared_edges(
        &mut self,
        edges: Vec<PreparedEdge>,
    ) -> Result<(), RiftError> {
        let maximum = usize::try_from(self.configuration.search.query.statements)
            .map_err(|_| unavailable("the accepted statement bound cannot be represented"))?;
        if self
            .references
            .query_provider_edges
            .len()
            .checked_add(edges.len())
            .is_none_or(|count| count > maximum)
        {
            return unavailable("query exceeds search.query.statements; narrow the query or increase the accepted bound").fail();
        }
        let mut package = Vec::new();
        for edge in edges {
            if self.provider_endpoint(&edge.to).is_none()
                || edge
                    .from
                    .as_ref()
                    .is_some_and(|target| self.provider_endpoint(target).is_none())
            {
                return unavailable("the captured endpoint has no original provider context")
                    .fail();
            }
            if self
                .references
                .query_package_facts
                .provider_endpoint(&edge.to)
                .is_some()
                && edge.from.as_ref().is_none_or(|target| {
                    self.references
                        .query_package_facts
                        .provider_endpoint(target)
                        .is_some()
                })
            {
                package.push(edge.clone());
            }
            self.references.query_provider_edges.push(edge);
        }
        project_provider_edges(&package, &mut self.references.query_package_facts)?;
        Ok(())
    }
}

pub(crate) fn project_provider_edges(
    edges: &[PreparedEdge],
    facts: &mut rift_search::query::QueryFacts,
) -> Result<(), RiftError> {
    for edge in edges {
        let from = edge
            .from
            .as_ref()
            .map(|target| {
                facts.provider_endpoint(target).ok_or_else(|| {
                    unavailable("the captured caller has no original provider context")
                })
            })
            .transpose()?;
        let to = facts
            .provider_endpoint(&edge.to)
            .ok_or_else(|| unavailable("the captured target has no original provider context"))?;
        facts.provider_edges.push(QueryProviderEdge {
            from,
            to,
            facet: edge.facet,
            occurrence: edge.occurrence.clone(),
            derivation: Some(rift_protocol::read::RelationshipDerivation::Resolution),
        });
    }
    deduplicate_provider_edges(&mut facts.provider_edges);
    Ok(())
}

struct PreparedSources<'source> {
    target: &'source QueryPackageTarget,
    facts: &'source rift_search::query::QueryFacts,
    captured_uri: &'source lsp_types::Uri,
    edges: Vec<PreparedEdge>,
    positions: BTreeMap<rift_protocol::read::SourceUnitId, rift_lsp::position::LineIndex<'source>>,
}

impl PreparedSources<'_> {
    fn package_callee(&mut self, pending: &crate::PackageCallee) -> Result<bool, RiftError> {
        let Some(selected) = provider_callee(self.target, pending, self.facts)? else {
            return Ok(false);
        };
        if self
            .facts
            .provider_symbol_index(&selected)
            .and_then(|index| self.facts.symbols.get(index))
            .is_some_and(|symbol| symbol.symbol.id.is_some())
        {
            return Ok(false);
        }
        let caller =
            QueryProviderTarget::canonical(&SymbolId(pending.caller().as_str().to_owned()))?;
        let occurrences = pending.query_sources().unwrap_or_default();
        if occurrences.is_empty() {
            self.edges.push(PreparedEdge {
                from: Some(caller),
                to: selected,
                facet: RelationshipFacet::Calls,
                occurrence: None,
            });
        } else {
            for occurrence in occurrences {
                self.edges.push(PreparedEdge {
                    from: Some(caller.clone()),
                    to: selected.clone(),
                    facet: RelationshipFacet::Calls,
                    occurrence: Some(occurrence.clone()),
                });
            }
        }
        Ok(true)
    }

    fn request(&mut self, request: &QuerySourceRequest) -> Result<bool, RiftError> {
        match request {
            QuerySourceRequest::IncomingCall {
                target,
                caller,
                ranges,
            } => self.incoming(target, caller, ranges),
            QuerySourceRequest::Reference {
                target,
                uri,
                range,
                encoding,
                facet,
                file_level,
                ..
            } => self.reference(target, uri, *range, *encoding, *facet, *file_level),
            QuerySourceRequest::Provider { .. } => self.supplied(request),
            QuerySourceRequest::Call(_) => Ok(false),
        }
    }

    fn incoming(
        &mut self,
        identity: &SymbolId,
        pending: &crate::PackageCallee,
        ranges: &[Range],
    ) -> Result<bool, RiftError> {
        if pending.query_uri() != Some(self.captured_uri) {
            return Ok(false);
        }
        let Some(selected) = provider_callee(self.target, pending, self.facts)? else {
            return Ok(false);
        };
        if self
            .facts
            .provider_symbol_index(&selected)
            .and_then(|index| self.facts.symbols.get(index))
            .is_some_and(|symbol| symbol.symbol.id.is_some())
        {
            return Ok(false);
        }
        let origin = pending
            .query_origin()
            .ok_or_else(|| unavailable("the caller has no exact dependency owner"))?;
        let unit = pending
            .unit(&origin)
            .ok_or_else(|| unavailable("the caller has no exact source unit"))?;
        let text = QueryPackageTarget::source_for(self.facts, &unit)?;
        let index = self
            .positions
            .entry(unit.clone())
            .or_insert_with(|| rift_lsp::position::LineIndex::new(text));
        let identity = QueryProviderTarget::canonical(identity)?;
        if ranges.is_empty() {
            self.edges.push(PreparedEdge {
                from: Some(selected),
                to: identity,
                facet: RelationshipFacet::Calls,
                occurrence: None,
            });
        } else {
            for range in ranges {
                self.edges.push(PreparedEdge {
                    from: Some(selected.clone()),
                    to: identity.clone(),
                    facet: RelationshipFacet::Calls,
                    occurrence: Some(SourceUnitSpan {
                        unit: unit.clone(),
                        range: indexed_range(index, *range, pending.encoding())?,
                    }),
                });
            }
        }
        Ok(true)
    }

    fn reference(
        &mut self,
        identity: &SymbolId,
        uri: &lsp_types::Uri,
        range: Range,
        encoding: PositionEncoding,
        facet: RelationshipFacet,
        file_level: bool,
    ) -> Result<bool, RiftError> {
        if uri != self.captured_uri {
            return Ok(false);
        }
        let text = QueryPackageTarget::source_for(self.facts, &self.target.unit)?;
        let index = self
            .positions
            .entry(self.target.unit.clone())
            .or_insert_with(|| rift_lsp::position::LineIndex::new(text));
        let Some(selected) =
            provider_at(self.target, self.facts, uri, range.start, encoding, false)?
        else {
            return Ok(false);
        };
        if file_level
            || self
                .facts
                .provider_symbol_index(&selected)
                .and_then(|index| self.facts.symbols.get(index))
                .is_some_and(|symbol| symbol.symbol.id.is_some())
        {
            return Ok(false);
        }
        self.edges.push(PreparedEdge {
            from: Some(selected),
            to: QueryProviderTarget::canonical(identity)?,
            facet,
            occurrence: Some(SourceUnitSpan {
                unit: self.target.unit.clone(),
                range: indexed_range(index, range, encoding)?,
            }),
        });
        Ok(true)
    }

    fn supplied(&mut self, request: &QuerySourceRequest) -> Result<bool, RiftError> {
        let QuerySourceRequest::Provider {
            subject,
            direction,
            facet,
            uri,
            range,
            encoding,
            name,
            ranges,
            sources,
            file_level,
            ..
        } = request
        else {
            return Ok(false);
        };
        if uri != self.captured_uri {
            return Ok(false);
        }
        let selected = provider_at(
            self.target,
            self.facts,
            uri,
            range.start,
            *encoding,
            name.is_some(),
        )?;
        if name.is_some() && selected.is_none() {
            return Ok(false);
        }
        match direction {
            TraversalDirection::Outgoing => {
                let selected = selected.ok_or_else(|| {
                    unavailable("the supplied outgoing target has no original declaration binding")
                })?;
                append_outgoing_edges(&mut self.edges, subject, selected, *facet, sources);
            }
            TraversalDirection::Incoming => {
                let text = QueryPackageTarget::source_for(self.facts, &self.target.unit)?;
                let index = self
                    .positions
                    .entry(self.target.unit.clone())
                    .or_insert_with(|| rift_lsp::position::LineIndex::new(text));
                let enclosing = if *file_level { None } else { selected };
                if ranges.is_empty() {
                    self.edges.push(PreparedEdge {
                        from: enclosing,
                        to: subject.clone(),
                        facet: *facet,
                        occurrence: None,
                    });
                } else {
                    for range in ranges {
                        self.edges.push(PreparedEdge {
                            from: enclosing.clone(),
                            to: subject.clone(),
                            facet: *facet,
                            occurrence: Some(SourceUnitSpan {
                                unit: self.target.unit.clone(),
                                range: indexed_range(index, *range, *encoding)?,
                            }),
                        });
                    }
                }
            }
        }
        Ok(true)
    }
}

fn append_outgoing_edges(
    edges: &mut Vec<PreparedEdge>,
    subject: &QueryProviderTarget,
    selected: QueryProviderTarget,
    facet: RelationshipFacet,
    sources: &[SourceUnitSpan],
) {
    if sources.is_empty() {
        edges.push(PreparedEdge {
            from: Some(subject.clone()),
            to: selected,
            facet,
            occurrence: None,
        });
    } else {
        for source in sources {
            edges.push(PreparedEdge {
                from: Some(subject.clone()),
                to: selected.clone(),
                facet,
                occurrence: Some(source.clone()),
            });
        }
    }
}

fn provider_callee(
    held: &QueryPackageTarget,
    callee: &super::super::PackageCallee,
    facts: &rift_search::query::QueryFacts,
) -> Result<Option<QueryProviderTarget>, RiftError> {
    let Some(origin) = callee.query_origin() else {
        return Ok(None);
    };
    if callee.unit(&origin).as_ref() != Some(&held.unit) {
        return Ok(None);
    }
    let Some(uri) = callee.query_uri() else {
        return Ok(None);
    };
    provider_at(held, facts, uri, callee.position(), callee.encoding(), true)
}

fn provider_at(
    held: &QueryPackageTarget,
    facts: &rift_search::query::QueryFacts,
    uri: &lsp_types::Uri,
    position: lsp_types::Position,
    encoding: PositionEncoding,
    name: bool,
) -> Result<Option<QueryProviderTarget>, RiftError> {
    let Some((scope, _)) = held
        .provider_names
        .iter()
        .find(|(_, declaration)| &declaration.uri == uri)
    else {
        return Ok(None);
    };
    let text = QueryPackageTarget::source_for(facts, &held.unit)?;
    let offset =
        u64::try_from(rift_lsp::position::LineIndex::new(text).byte_offset(encoding, position)?)
            .map_err(|_| unavailable("the dependency source position cannot be represented"))?;
    if name {
        return Ok(held
            .provider_names
            .iter()
            .find(|(_, declaration)| {
                &declaration.uri == uri
                    && declaration.source.range.start <= offset
                    && offset < declaration.source.range.end
            })
            .map(|(target, _)| target.clone()));
    }
    let Some(context) = &scope.context else {
        return Ok(None);
    };
    let Some(snapshot) = facts
        .providers
        .iter()
        .find(|snapshot| &snapshot.context == context.as_ref())
    else {
        return Ok(None);
    };
    Ok(snapshot
        .records
        .iter()
        .flat_map(|record| {
            record
                .declarations
                .iter()
                .map(move |declaration| (record, declaration))
        })
        .filter(|(_, declaration)| {
            declaration.binding.unit().to_string() == held.unit.as_str()
                && declaration.binding.range().start() <= offset
                && offset < declaration.binding.range().end()
        })
        .min_by_key(|(_, declaration)| {
            declaration.binding.range().end() - declaration.binding.range().start()
        })
        .and_then(|(record, _)| record.target())
        .map(|target| QueryProviderTarget {
            context: Some(Arc::clone(context)),
            target,
        }))
}

fn validate_original_record(
    selected: &rift_index::WorkspaceSymbolView,
    record: &rift_search::query::QueryProviderRecord,
) -> Result<(), RiftError> {
    let graph = selected.normalized_graph();
    let original = record
        .contributions
        .first()
        .and_then(|key| graph.record_for(key.reference()))
        .ok_or_else(|| unavailable("the physical subject has no original provider association"))?;
    if original.identity() != record.identity.as_ref()
        || original.contributions() != record.contributions.as_slice()
    {
        return unavailable("the physical subject has no original provider association").fail();
    }
    for declaration in &record.declarations {
        if graph
            .contribution(&declaration.contribution)
            .and_then(|contribution| contribution.source())
            != Some(&declaration.binding)
        {
            return unavailable("the physical subject has no original provider association").fail();
        }
    }
    Ok(())
}
