//! Physical dependency targets retained by the request's original normalized owner.

use super::*;
use rift_search::query::{QueryProviderEdge, QueryProviderTarget};

pub(super) struct PreparedEdge {
    from: Option<QueryProviderTarget>,
    to: QueryProviderTarget,
    facet: RelationshipFacet,
    occurrence: Option<SourceUnitSpan>,
}

impl EngineQuery<'_> {
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
        for subject in subjects {
            if let Some(identity) = subject.canonical_symbol() {
                self.resolve_facets(&[identity], direction, facets).await?;
                continue;
            }
            for facet in facets {
                let endpoint = self
                    .references
                    .query_package_facts
                    .provider_endpoint(subject)
                    .ok_or_else(|| {
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

    async fn resolve_physical(
        &mut self,
        subject: &QueryProviderTarget,
        direction: TraversalDirection,
        facet: RelationshipFacet,
    ) -> Result<(), RiftError> {
        let (held, declaration) = self
            .provider_targets
            .iter()
            .find_map(|held| {
                held.provider_names
                    .iter()
                    .find(|(target, _)| target == subject)
                    .map(|(_, declaration)| (Arc::clone(held), declaration))
            })
            .ok_or_else(|| {
                unavailable("the physical subject has no captured declaration name position")
            })?;
        let slot = self
            .engines
            .engine_for(&held.provider)
            .ok_or_else(|| unavailable("the admitted dependency provider is unavailable"))?;
        let source = held.source_for(
            &self.references.query_package_facts,
            &declaration.source.unit,
        )?;
        let positions = rift_lsp::position::LineIndex::new(source);
        let offset = usize::try_from(declaration.source.range.start)
            .map_err(|_| unavailable("the physical declaration position cannot be represented"))?;
        let target = ReferenceTarget {
            language: held.provider.name.clone(),
            source: source.to_owned(),
            utf8: positions.position(PositionEncoding::Utf8, offset)?,
            utf16: positions.position(PositionEncoding::Utf16, offset)?,
            uri: declaration.uri.clone(),
            canonical_uri: declaration.uri.clone(),
        };
        let maximum = usize::try_from(self.configuration.search.query.statements)
            .map_err(|_| unavailable("the accepted statement bound cannot be represented"))?;
        let mut pending = Vec::new();
        let mut edges = Vec::new();
        let unconfirmed = match (direction, facet) {
            (TraversalDirection::Outgoing, RelationshipFacet::Calls) => {
                let (report, unconfirmed) = outgoing_report(
                    Box::pin(outgoing_on_engine(
                        slot,
                        &target,
                        self.deadline,
                        maximum,
                        self.provider_requests.clone(),
                    ))
                    .await?,
                )?;
                let count = report.calls.iter().try_fold(0usize, |count, call| {
                    count.checked_add(call.from_ranges.len().max(1))
                });
                if count.is_none_or(|count| count > maximum) {
                    return unavailable(
                        "the outgoing occurrences exceed the engine response bound",
                    )
                    .fail();
                }
                for call in report.calls {
                    let sources = call
                        .from_ranges
                        .into_iter()
                        .map(|range| {
                            indexed_range(&positions, range, report.encoding).map(|range| {
                                SourceUnitSpan {
                                    unit: declaration.source.unit.clone(),
                                    range,
                                }
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    self.map_provider_location(
                        QuerySourceRequest::Provider {
                            subject: subject.clone(),
                            direction,
                            facet,
                            provider: held.provider.clone(),
                            uri: call.to.uri,
                            range: call.to.selection_range,
                            encoding: report.encoding,
                            name: Some(call.to.name),
                            ranges: Vec::new(),
                            sources,
                            file_level: false,
                        },
                        &mut pending,
                        &mut edges,
                    )?;
                }
                unconfirmed
            }
            (TraversalDirection::Incoming, RelationshipFacet::Calls) => {
                let (report, unconfirmed) = outgoing_report(
                    Box::pin(incoming_on_engine(
                        slot,
                        &target,
                        self.deadline,
                        maximum,
                        self.provider_requests.clone(),
                    ))
                    .await?,
                )?;
                let count = report.calls.iter().try_fold(0usize, |count, call| {
                    count.checked_add(call.from_ranges.len().max(1))
                });
                if count.is_none_or(|count| count > maximum) {
                    return unavailable(
                        "the incoming call occurrences exceed the engine response bound",
                    )
                    .fail();
                }
                for call in report.calls {
                    let file_level = call.from.kind == lsp_types::SymbolKind::MODULE
                        && call.from.range == Range::default()
                        && call.from.selection_range == Range::default();
                    self.map_provider_location(
                        QuerySourceRequest::Provider {
                            subject: subject.clone(),
                            direction,
                            facet,
                            provider: held.provider.clone(),
                            uri: call.from.uri,
                            range: call.from.selection_range,
                            encoding: report.encoding,
                            name: (!file_level).then_some(call.from.name),
                            ranges: call.from_ranges,
                            sources: Vec::new(),
                            file_level,
                        },
                        &mut pending,
                        &mut edges,
                    )?;
                }
                unconfirmed
            }
            (TraversalDirection::Incoming, RelationshipFacet::References) => {
                let (report, unconfirmed) = Box::pin(references_on_engine_with_bound(
                    slot,
                    &target,
                    self.deadline,
                    maximum,
                    Some(self.provider_requests.clone()),
                ))
                .await?;
                if !report.full {
                    return unavailable("the reference answer did not establish the declaration")
                        .fail();
                }
                for location in report.locations.map_err(|_| {
                    unavailable("the reference answer exceeds the engine response bound")
                })? {
                    if location.uri == declaration.uri
                        && indexed_range(&positions, location.range, report.encoding)?
                            == declaration.source.range
                    {
                        continue;
                    }
                    self.map_provider_location(
                        QuerySourceRequest::Provider {
                            subject: subject.clone(),
                            direction,
                            facet,
                            provider: held.provider.clone(),
                            uri: location.uri,
                            range: location.range,
                            encoding: report.encoding,
                            name: None,
                            ranges: vec![location.range],
                            sources: Vec::new(),
                            file_level: false,
                        },
                        &mut pending,
                        &mut edges,
                    )?;
                }
                unconfirmed
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
        let file = self
            .reads
            .index()
            .file(&path)
            .ok_or_else(|| unavailable("the engine target has no selected source bytes"))?;
        let unit = crate::read::source_unit_id(file.path());
        let positions = rift_lsp::position::LineIndex::new(file.source());
        let selection = indexed_range(&positions, *range, *encoding)?;
        let selected = if *file_level {
            None
        } else if name.is_some() {
            Some(
                project_declaration(self.reads, &path, (*range, *encoding), "query calls")?
                    .ok_or_else(|| {
                        unavailable("the supplied call target has no selected declaration")
                    })?,
            )
        } else {
            file.enclosing_symbol(selection.start, selection.end)
                .map(|symbol| symbol_id(self.reads.index(), crate::search::declared(file, symbol)))
                .transpose()?
                .flatten()
        }
        .as_ref()
        .map(QueryProviderTarget::canonical)
        .transpose()?;
        match direction {
            TraversalDirection::Outgoing => {
                let selected = selected.ok_or_else(|| {
                    unavailable("the outgoing target has no selected declaration")
                })?;
                if sources.is_empty() {
                    edges.push(PreparedEdge {
                        from: Some(subject.clone()),
                        to: selected,
                        facet: *facet,
                        occurrence: None,
                    });
                } else {
                    for source in sources {
                        edges.push(PreparedEdge {
                            from: Some(subject.clone()),
                            to: selected.clone(),
                            facet: *facet,
                            occurrence: Some(source.clone()),
                        });
                    }
                }
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
        let facts = &self.references.query_package_facts;
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
        for edge in &facts.provider_edges {
            follow(edge.from.as_ref(), &edge.to, edge.facet);
        }
        for edge in self.references.query_relationships() {
            let from = facts
                .provider_endpoint(&QueryProviderTarget::canonical(&edge.from)?)
                .ok_or_else(|| unavailable("the relationship caller has no canonical identity"))?;
            let to = facts
                .provider_endpoint(&QueryProviderTarget::canonical(&edge.to)?)
                .ok_or_else(|| unavailable("the relationship target has no canonical identity"))?;
            for facet in edge.facets {
                follow(Some(&from), &to, facet);
            }
        }
        for (provider, snapshot) in facts.providers.iter().enumerate() {
            let endpoint = |target| rift_search::query::QueryProviderEndpoint {
                provider: Some(provider),
                target,
            };
            let source = |key: &rift_core::ContributionKey| {
                snapshot
                    .records
                    .iter()
                    .find(|record| record.contributions.contains(key))
                    .and_then(|record| record.target())
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
        let mut edges = Vec::new();
        let mut waiting = Vec::new();
        for pending in std::mem::take(&mut self.references.package_callees) {
            let Some(selected) = provider_callee(target, &pending, facts)? else {
                waiting.push(pending);
                continue;
            };
            if facts
                .provider_symbol_index(&selected)
                .and_then(|index| facts.symbols.get(index))
                .is_some_and(|symbol| symbol.symbol.id.is_some())
            {
                waiting.push(pending);
                continue;
            }
            let caller =
                QueryProviderTarget::canonical(&SymbolId(pending.caller().as_str().to_owned()))?;
            let occurrences = pending.query_sources().unwrap_or_default();
            if occurrences.is_empty() {
                edges.push(PreparedEdge {
                    from: Some(caller),
                    to: selected,
                    facet: RelationshipFacet::Calls,
                    occurrence: None,
                });
            } else {
                for occurrence in occurrences {
                    edges.push(PreparedEdge {
                        from: Some(caller.clone()),
                        to: selected.clone(),
                        facet: RelationshipFacet::Calls,
                        occurrence: Some(occurrence.clone()),
                    });
                }
            }
        }
        self.references.package_callees = waiting;
        let mut waiting = Vec::new();
        let mut positions = BTreeMap::new();
        for request in std::mem::take(&mut self.references.query_source_requests) {
            match &request {
                QuerySourceRequest::IncomingCall {
                    target: called,
                    caller,
                    ranges,
                } => {
                    if caller.query_uri() != Some(captured_uri) {
                        waiting.push(request);
                        continue;
                    }
                    let Some(selected) = provider_callee(target, caller, facts)? else {
                        waiting.push(request);
                        continue;
                    };
                    if facts
                        .provider_symbol_index(&selected)
                        .and_then(|index| facts.symbols.get(index))
                        .is_some_and(|symbol| symbol.symbol.id.is_some())
                    {
                        waiting.push(request);
                        continue;
                    }
                    let origin = caller
                        .query_origin()
                        .ok_or_else(|| unavailable("the caller has no exact dependency owner"))?;
                    let unit = caller
                        .unit(&origin)
                        .ok_or_else(|| unavailable("the caller has no exact source unit"))?;
                    let text = target.source_for(facts, &unit)?;
                    let index = positions
                        .entry(unit.clone())
                        .or_insert_with(|| rift_lsp::position::LineIndex::new(text));
                    let called = QueryProviderTarget::canonical(called)?;
                    if ranges.is_empty() {
                        edges.push(PreparedEdge {
                            from: Some(selected),
                            to: called,
                            facet: RelationshipFacet::Calls,
                            occurrence: None,
                        });
                    } else {
                        for range in ranges {
                            edges.push(PreparedEdge {
                                from: Some(selected.clone()),
                                to: called.clone(),
                                facet: RelationshipFacet::Calls,
                                occurrence: Some(SourceUnitSpan {
                                    unit: unit.clone(),
                                    range: indexed_range(index, *range, caller.encoding())?,
                                }),
                            });
                        }
                    }
                }
                QuerySourceRequest::Reference {
                    target: called,
                    uri,
                    range,
                    encoding,
                    facet,
                    file_level,
                    ..
                } => {
                    if uri != captured_uri {
                        waiting.push(request);
                        continue;
                    }
                    let text = target.source_for(facts, &target.unit)?;
                    let index = positions
                        .entry(target.unit.clone())
                        .or_insert_with(|| rift_lsp::position::LineIndex::new(text));
                    let Some(selected) =
                        provider_at(target, facts, uri, range.start, *encoding, false)?
                    else {
                        waiting.push(request);
                        continue;
                    };
                    if *file_level
                        || facts
                            .provider_symbol_index(&selected)
                            .and_then(|index| facts.symbols.get(index))
                            .is_some_and(|symbol| symbol.symbol.id.is_some())
                    {
                        waiting.push(request);
                        continue;
                    }
                    edges.push(PreparedEdge {
                        from: Some(selected),
                        to: QueryProviderTarget::canonical(called)?,
                        facet: *facet,
                        occurrence: Some(SourceUnitSpan {
                            unit: target.unit.clone(),
                            range: indexed_range(index, *range, *encoding)?,
                        }),
                    });
                }
                QuerySourceRequest::Provider {
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
                } => {
                    if uri != captured_uri {
                        waiting.push(request);
                        continue;
                    }
                    let selected =
                        provider_at(target, facts, uri, range.start, *encoding, name.is_some())?;
                    if name.is_some() && selected.is_none() {
                        waiting.push(request);
                        continue;
                    }
                    match direction {
                        TraversalDirection::Outgoing => {
                            let selected = selected.ok_or_else(|| unavailable("the supplied outgoing target has no original declaration binding"))?;
                            if sources.is_empty() {
                                edges.push(PreparedEdge {
                                    from: Some(subject.clone()),
                                    to: selected,
                                    facet: *facet,
                                    occurrence: None,
                                });
                            } else {
                                for source in sources {
                                    edges.push(PreparedEdge {
                                        from: Some(subject.clone()),
                                        to: selected.clone(),
                                        facet: *facet,
                                        occurrence: Some(source.clone()),
                                    });
                                }
                            }
                        }
                        TraversalDirection::Incoming => {
                            let text = target.source_for(facts, &target.unit)?;
                            let index = positions
                                .entry(target.unit.clone())
                                .or_insert_with(|| rift_lsp::position::LineIndex::new(text));
                            let enclosing = if *file_level { None } else { selected };
                            if ranges.is_empty() {
                                edges.push(PreparedEdge {
                                    from: enclosing,
                                    to: subject.clone(),
                                    facet: *facet,
                                    occurrence: None,
                                });
                            } else {
                                for range in ranges {
                                    edges.push(PreparedEdge {
                                        from: enclosing.clone(),
                                        to: subject.clone(),
                                        facet: *facet,
                                        occurrence: Some(SourceUnitSpan {
                                            unit: target.unit.clone(),
                                            range: indexed_range(index, *range, *encoding)?,
                                        }),
                                    });
                                }
                            }
                        }
                    }
                }
                QuerySourceRequest::Call(_) => waiting.push(request),
            }
        }
        self.references.query_source_requests = waiting;
        Ok(edges)
    }

    pub(super) fn admit_prepared_edges(
        &mut self,
        edges: Vec<PreparedEdge>,
    ) -> Result<(), RiftError> {
        let facts = &mut self.references.query_package_facts;
        let maximum = usize::try_from(self.configuration.search.query.statements)
            .map_err(|_| unavailable("the accepted statement bound cannot be represented"))?;
        if facts
            .provider_edges
            .len()
            .checked_add(edges.len())
            .is_none_or(|count| count > maximum)
        {
            return unavailable("query exceeds search.query.statements; narrow the query or increase the accepted bound").fail();
        }
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
            let to = facts.provider_endpoint(&edge.to).ok_or_else(|| {
                unavailable("the captured target has no original provider context")
            })?;
            facts.provider_edges.push(QueryProviderEdge {
                from,
                to,
                facet: edge.facet,
                occurrence: edge.occurrence,
                derivation: Some(rift_protocol::read::RelationshipDerivation::Resolution),
            });
        }
        deduplicate_provider_edges(&mut facts.provider_edges);
        Ok(())
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
    let text = held.source_for(facts, &held.unit)?;
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
