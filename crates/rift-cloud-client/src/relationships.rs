//! Raw adjacency through the existing bounded global search transport.

use std::collections::HashSet;

use rift_error::RiftError;
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::relationship_read::{
    RELATIONSHIPS_FEATURE, RelationshipBounds, RelationshipCapability, RelationshipSearchPage,
    RelationshipSearchRequest, RelationshipSearchResult,
};

use crate::{GlobalClient, SearchPackagesRequestQuery, contract, failure, response};

mod facts;

/// Accepted operation bounds supplied by the shared query acquisition owner.
#[derive(Clone, Copy, Debug)]
pub struct RelationshipReadLimits {
    /// Most HTTP pages acquired for this batch.
    pub requests_max: u64,
    /// Most original relationship records retained across all pages.
    pub records_max: u64,
    /// Most serialized relationship bytes retained across all pages.
    pub bytes_max: u64,
}

impl GlobalClient {
    /// Reads the exact direction and facets supported by the advertised reader.
    /// Older metadata without this optional field establishes no relationship operation.
    ///
    /// # Errors
    /// Returns invalid response metadata for duplicate directions, empty or repeated facets.
    pub fn relationship_capabilities(
        capabilities: &crate::Capabilities,
    ) -> Result<Vec<RelationshipCapability>, RiftError> {
        let Some(operations) = capabilities.relationship_capabilities.as_ref() else {
            return Ok(Vec::new());
        };
        let supported = operations
            .iter()
            .map(|operation| {
                serde_json::from_value::<RelationshipCapability>(serde_json::json!({
                    "direction": operation.direction,
                    "facets": operation.facets,
                }))
                .map(|mut selected| {
                    selected.bounds = operation.bounds.as_ref().map(|bounds| RelationshipBounds {
                        max_seeds: bounds.max_seeds,
                        max_facets: bounds.max_facets,
                        max_relationships: bounds.max_relationships,
                        max_occurrences: bounds.max_occurrences,
                        max_symbols: bounds.max_symbols,
                        max_sources: bounds.max_sources,
                    });
                    selected
                })
                .map_err(|_| invalid("relationship capabilities are invalid"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if supported.len() > 2 {
            return Err(invalid("relationship capabilities repeat a direction"));
        }
        if !supported.is_empty() && !crate::supports_feature(capabilities, RELATIONSHIPS_FEATURE) {
            return Err(invalid(
                "relationship operation metadata differs from the advertised feature",
            ));
        }
        for (index, operation) in supported.iter().enumerate() {
            if operation.facets.is_empty()
                || operation.bounds.as_ref().is_some_and(|bounds| {
                    [
                        bounds.max_seeds,
                        bounds.max_facets,
                        bounds.max_relationships,
                        bounds.max_occurrences,
                        bounds.max_symbols,
                        bounds.max_sources,
                    ]
                    .contains(&0)
                })
                || supported[..index]
                    .iter()
                    .any(|previous| previous.direction == operation.direction)
                || operation
                    .facets
                    .iter()
                    .enumerate()
                    .any(|(position, facet)| operation.facets[..position].contains(facet))
            {
                return Err(invalid(
                    "relationship capabilities repeat or omit a required selection",
                ));
            }
        }
        Ok(supported)
    }

    /// Intersects the accepted local counts with the advertised raw operation bounds.
    ///
    /// # Errors
    /// Returns unsupported when the exact raw operation or its count policy is absent.
    pub fn relationship_bounds(
        &self,
        capabilities: &crate::Capabilities,
        direction: rift_protocol::read::TraversalDirection,
        facets: &[rift_protocol::read::RelationshipFacet],
    ) -> Result<RelationshipBounds, RiftError> {
        let selected = Self::relationship_capabilities(capabilities)?
            .into_iter()
            .find(|operation| {
                operation.direction == direction
                    && !facets.is_empty()
                    && facets.iter().all(|facet| operation.facets.contains(facet))
            });
        let bounds = selected
            .and_then(|operation| operation.bounds)
            .ok_or_else(|| {
                rift_error::errors::server::read_unsupported()
                    .capability("global relationship operation with advertised count bounds")
                    .error()
            })?;
        // A local usize above u64::MAX cannot restrict an advertised u64 count.
        let count = |remote: u64, local: usize| {
            u64::try_from(local).map_or(remote, |local| remote.min(local))
        };
        let config = &self.inner.config;
        Ok(RelationshipBounds {
            max_seeds: count(bounds.max_seeds, config.max_seeds),
            max_facets: count(bounds.max_facets, config.max_facets),
            max_relationships: count(bounds.max_relationships, config.max_relationships),
            max_occurrences: count(bounds.max_occurrences, config.max_occurrences),
            max_symbols: count(bounds.max_symbols, config.max_symbols),
            max_sources: count(bounds.max_sources, config.max_sources),
        })
    }

    /// Reads one raw adjacency page without ranking, endpoint deduplication or shortest paths.
    ///
    /// # Errors
    /// Returns request, capability, transport or strict response validation failure.
    pub async fn relationship_page(
        &self,
        request: &RelationshipSearchRequest,
        limit: i64,
        cursor: Option<&str>,
    ) -> Result<RelationshipSearchResult, RiftError> {
        validate_request(request, &self.inner.config)?;
        crate::validate_page(limit, cursor)?;
        let capabilities = self.get_capabilities().await?;
        if !crate::supports_feature(&capabilities, RELATIONSHIPS_FEATURE) {
            return Err(failure::configuration_invalid(
                "feature",
                "global API does not advertise feature: relationships",
            ));
        }
        let bounds = self.validate_advertised_request(request, &capabilities)?;
        crate::validate_read_bounds(
            request
                .packages
                .len()
                .saturating_add(request.runtimes.len()),
            limit,
            cursor,
            &capabilities,
        )?;
        let body = serialize_request(request, self.inner.config.max_request)?;
        crate::validate_body_for_capabilities(&body, &capabilities, self.inner.config.max_request)?;
        let query = SearchPackagesRequestQuery {
            limit: Some(limit),
            cursor: cursor.map(str::to_owned),
        };
        let raw = match self
            .request_with_query(
                contract::Endpoint::Search,
                body,
                &query,
                crate::active_response_body_bytes_max(
                    &capabilities,
                    self.inner.config.max_response,
                ),
            )
            .await
        {
            Ok(raw) => raw,
            Err(error) => return self.observed(Err(error)).await,
        };
        let parsed = match response::exact_response(
            raw,
            contract::Endpoint::Search.operation_id(),
            &[200, 503],
            |status, value| {
                matches!(
                    (status, value),
                    (200, RelationshipSearchResult::Page(_))
                        | (503, RelationshipSearchResult::Unavailable(_))
                )
            },
        ) {
            Ok(parsed) => parsed,
            Err(error) => return self.observed(Err(error)).await,
        };
        if let RelationshipSearchResult::Page(page) = &parsed.value {
            if let Err(error) = validate_page(request, page, limit, &bounds) {
                return self.observed(Err(error)).await;
            }
            if let Err(error) = facts::validate_page(
                request,
                page,
                crate::smaller_bound(
                    capabilities.bounds.source_bytes_max,
                    self.inner.config.max_source,
                ),
                usize::try_from(bounds.max_symbols)
                    .map_err(|_| invalid("relationship symbol count exceeds the platform bound"))?,
                usize::try_from(bounds.max_sources)
                    .map_err(|_| invalid("relationship source count exceeds the platform bound"))?,
            ) {
                return self.observed(Err(error)).await;
            }
        }
        Ok(parsed.value)
    }

    fn validate_advertised_request(
        &self,
        request: &RelationshipSearchRequest,
        capabilities: &crate::Capabilities,
    ) -> Result<RelationshipBounds, RiftError> {
        let supported = Self::relationship_capabilities(capabilities)?;
        let bounds = self.relationship_bounds(
            capabilities,
            request.relationships.direction,
            &request.relationships.facets,
        )?;
        if !count_within(request.relationships.seeds.len(), bounds.max_seeds)
            || !count_within(request.relationships.facets.len(), bounds.max_facets)
        {
            return Err(failure::request_invalid(
                "relationship_page",
                "relationships",
                "relationship selection exceeds its advertised count bound",
            ));
        }
        if !supported.iter().any(|operation| {
            operation.direction == request.relationships.direction
                && !request.relationships.facets.is_empty()
                && request
                    .relationships
                    .facets
                    .iter()
                    .all(|facet| operation.facets.contains(facet))
        }) {
            return Err(failure::configuration_invalid(
                "relationships",
                "global API does not support the required relationship direction and facets",
            ));
        }
        Ok(bounds)
    }

    /// Acquires every required cursor page under original context and explicit work bounds.
    ///
    /// # Errors
    /// Returns unavailable or validation failure without exposing a partial adjacency.
    pub async fn relationship_pages(
        &self,
        request: &RelationshipSearchRequest,
        limits: RelationshipReadLimits,
    ) -> Result<RelationshipSearchResult, RiftError> {
        self.relationship_pages_with_admission(request, limits, || Ok(()))
            .await
    }

    /// Acquires every cursor page after admission by the request's shared work owner.
    ///
    /// # Errors
    /// Returns admission, unavailable or strict response validation failure.
    pub async fn relationship_pages_with_admission(
        &self,
        request: &RelationshipSearchRequest,
        limits: RelationshipReadLimits,
        mut admit: impl FnMut() -> Result<(), RiftError>,
    ) -> Result<RelationshipSearchResult, RiftError> {
        validate_request(request, &self.inner.config)?;
        let capabilities = self.get_capabilities().await?;
        let limit = crate::advertised_page_limit(crate::PAGE_LIMIT_MAX, &capabilities);
        let mut continuation_request = None;
        let mut result: Option<RelationshipSearchPage> = None;
        let mut cursor = None;
        let mut cursors = HashSet::new();
        let mut records = 0_u64;
        let mut record_ids = HashSet::new();
        let mut bytes = 0_u64;
        for _ in 0..limits.requests_max {
            admit()?;
            let page = match self
                .relationship_page(
                    continuation_request.as_ref().unwrap_or(request),
                    limit,
                    cursor.as_deref(),
                )
                .await?
            {
                RelationshipSearchResult::Page(page) => page,
                unavailable @ RelationshipSearchResult::Unavailable(_) => return Ok(unavailable),
            };
            let count = page
                .groups
                .iter()
                .map(|group| group.relationships.len() + group.occurrences.len())
                .sum::<usize>();
            let completion_progress = completion_progress(result.as_ref(), &page);
            let identities = page_record_ids(&page)?;
            records =
                records
                    .checked_add(u64::try_from(identities.len()).map_err(|_| {
                        invalid("relationship record count exceeds the platform bound")
                    })?)
                    .filter(|count| *count <= limits.records_max)
                    .ok_or_else(|| {
                        invalid("relationship acquisition exceeds the accepted record bound")
                    })?;
            for identity in identities {
                if !record_ids.insert(identity.to_owned()) {
                    return Err(invalid(
                        "relationship record identity repeats across cursor pages",
                    ));
                }
            }
            admit_page_bytes(&page, &mut bytes, limits.bytes_max)?;
            let next = page.next_cursor.clone();
            if let Some(selected) = &mut result {
                merge_page(selected, page)?;
            } else {
                result = Some(page);
            }
            let Some(next) = next else {
                let selected = result.ok_or_else(|| invalid("relationship page is absent"))?;
                if selected.groups.iter().any(|group| !group.complete) {
                    return Err(invalid(
                        "final relationship page did not complete every requested subject",
                    ));
                }
                facts::validate_complete(request, &selected)?;
                return Ok(RelationshipSearchResult::Page(selected));
            };
            if (count == 0 && !completion_progress) || !cursors.insert(next.clone()) {
                return Err(invalid("relationship cursor made no progress"));
            }
            if continuation_request.is_none() {
                let selected = result
                    .as_ref()
                    .ok_or_else(|| invalid("relationship page is absent"))?;
                let mut continued = request.clone();
                continued.view = Some(selected.view.id.clone());
                continuation_request = Some(continued);
            }
            cursor = Some(next);
        }
        Err(invalid(
            "relationship acquisition exceeds the accepted request bound",
        ))
    }
}

fn serialize_request(
    request: &RelationshipSearchRequest,
    limit: usize,
) -> Result<Vec<u8>, RiftError> {
    let mut buffer = rift_protocol::message::MessageBuffer::new(limit);
    if let Err(error) = serde_json::to_writer(&mut buffer, request) {
        return Err(match buffer.rejected_bytes() {
            Some(bytes) => failure::body_limit("serialize_body", "request", bytes, limit),
            None => failure::request_invalid_source(
                "serialize_body",
                "body",
                "JSON serialization failed",
                error,
            ),
        });
    }
    Ok(buffer.into_bytes())
}

fn admit_page_bytes(
    page: &RelationshipSearchPage,
    bytes: &mut u64,
    maximum: u64,
) -> Result<(), RiftError> {
    let serialized = page
        .groups
        .iter()
        .flat_map(|group| {
            group
                .relationships
                .iter()
                .map(serde_json::to_vec)
                .chain(group.occurrences.iter().map(serde_json::to_vec))
        })
        .chain(page.symbols.iter().map(serde_json::to_vec))
        .chain(page.sources.iter().map(serde_json::to_vec));
    for encoded in serialized {
        let encoded = encoded.map_err(|source| {
            failure::response_source(
                "relationship_pages",
                "relationships",
                "JSON serialization failed",
                None,
                source,
            )
        })?;
        *bytes = bytes
            .checked_add(
                u64::try_from(encoded.len())
                    .map_err(|_| invalid("relationship byte count exceeds the platform bound"))?,
            )
            .ok_or_else(|| invalid("relationship byte count exceeds the platform bound"))?;
        if *bytes > maximum {
            return Err(invalid(
                "relationship acquisition exceeds the accepted record or byte bound",
            ));
        }
    }
    Ok(())
}

fn completion_progress(
    selected: Option<&RelationshipSearchPage>,
    page: &RelationshipSearchPage,
) -> bool {
    match selected {
        Some(selected) => selected
            .groups
            .iter()
            .zip(&page.groups)
            .any(|(previous, group)| !previous.complete && group.complete),
        None => page.groups.iter().any(|group| group.complete),
    }
}

fn validate_request(
    request: &RelationshipSearchRequest,
    config: &crate::Config,
) -> Result<(), RiftError> {
    let selection = &request.relationships;
    if selection.seeds.is_empty()
        || selection.seeds.len() > config.max_seeds
        || selection.facets.len() > config.max_facets
        || (request.packages.is_empty() && request.runtimes.is_empty() && request.view.is_none())
        || request
            .packages
            .len()
            .saturating_add(request.runtimes.len())
            > crate::PACKAGES_MAX
        || request.include.len() > 1
        || request
            .include
            .iter()
            .any(|include| *include != rift_protocol::read::GetSymbolInclude::Source)
    {
        return Err(failure::request_invalid(
            "relationship_page",
            "relationships",
            "raw adjacency requires bounded subjects and an exact package domain or retained view",
        ));
    }
    validate_runtimes(request)?;
    let mut seeds = HashSet::new();
    if selection
        .facets
        .iter()
        .enumerate()
        .any(|(index, facet)| selection.facets[..index].contains(facet))
    {
        return Err(failure::request_invalid(
            "relationship_page",
            "relationships.facets",
            "facets must be distinct",
        ));
    }
    for seed in &selection.seeds {
        let identity = SymbolIdentity::parse(seed.as_str()).map_err(|source| {
            failure::request_invalid_source(
                "relationship_page",
                "relationships.seeds",
                "subject must be a canonical symbol identity",
                source,
            )
        })?;
        if !matches!(
            identity.owner(),
            rift_protocol::identity::SymbolOwner::Package { .. }
                | rift_protocol::identity::SymbolOwner::Runtime { .. }
        ) {
            return Err(failure::request_invalid(
                "relationship_page",
                "relationships.seeds",
                "subject must have package or runtime ownership",
            ));
        }
        if !seeds.insert(seed.as_str()) {
            return Err(failure::request_invalid(
                "relationship_page",
                "relationships.seeds",
                "subjects must be distinct",
            ));
        }
    }
    validate_packages(request)
}

fn validate_runtimes(request: &RelationshipSearchRequest) -> Result<(), RiftError> {
    let mut runtimes = HashSet::new();
    for runtime in &request.runtimes {
        rift_protocol::identity::SymbolOwner::Runtime {
            runtime: runtime.runtime.clone(),
            version: runtime.version.clone(),
        }
        .validate()?;
        if !runtimes.insert((&runtime.runtime, &runtime.version)) {
            return Err(failure::request_invalid(
                "relationship_page",
                "runtimes",
                "exact runtimes must be distinct",
            ));
        }
    }
    Ok(())
}

fn validate_packages(request: &RelationshipSearchRequest) -> Result<(), RiftError> {
    let mut packages = HashSet::new();
    for package in &request.packages {
        package.owner()?;
        for (value, maximum) in [
            (&package.manager, crate::PACKAGE_MANAGER_CHARS_MAX),
            (&package.registry, crate::PACKAGE_REGISTRY_CHARS_MAX),
            (&package.name, crate::PACKAGE_NAME_CHARS_MAX),
            (&package.version, crate::PACKAGE_VERSION_CHARS_MAX),
        ] {
            if !crate::within_characters(value, maximum) {
                return Err(failure::request_invalid(
                    "relationship_page",
                    "packages",
                    "exact package identity exceeds the accepted bound",
                ));
            }
        }
        if !packages.insert((
            &package.manager,
            &package.registry,
            &package.name,
            &package.version,
        )) {
            return Err(failure::request_invalid(
                "relationship_page",
                "packages",
                "exact packages must be distinct",
            ));
        }
    }
    Ok(())
}

fn validate_page(
    request: &RelationshipSearchRequest,
    page: &RelationshipSearchPage,
    limit: i64,
    bounds: &RelationshipBounds,
) -> Result<(), RiftError> {
    if page.groups.len() != request.relationships.seeds.len()
        || page.analyzer_revision.is_empty()
        || page.analyzer_revision.len() > crate::LABEL_CHARS_MAX
        || page.corpus_revision.is_empty()
        || page.corpus_revision.len() > crate::LABEL_CHARS_MAX
        || page.warnings.len() > crate::WARNINGS_MAX
    {
        return Err(invalid(
            "relationship page metadata or requested groups differ",
        ));
    }
    let count = page_record_ids(page)?.len();
    for (seed, group) in request.relationships.seeds.iter().zip(&page.groups) {
        if seed != &group.seed {
            return Err(invalid(
                "relationship page omitted or reordered a requested subject",
            ));
        }
        if !count_within(group.relationships.len(), bounds.max_relationships)
            || !count_within(group.occurrences.len(), bounds.max_occurrences)
        {
            return Err(invalid("relationship projections exceed their page bound"));
        }
        for record in &group.occurrences {
            let occurrence = &record.occurrence;
            SymbolIdentity::parse(occurrence.target.as_str())?;
            if let Some(enclosing) = &occurrence.enclosing {
                SymbolIdentity::parse(enclosing.as_str())?;
            }
            rift_core::SourceUnitId::parse(occurrence.source.unit.as_str())?;
            let matches = match request.relationships.direction {
                rift_protocol::read::TraversalDirection::Incoming => &occurrence.target == seed,
                rift_protocol::read::TraversalDirection::Outgoing => {
                    occurrence.enclosing.as_ref() == Some(seed)
                }
            };
            if !matches
                || occurrence.source.range.start > occurrence.source.range.end
                || occurrence.source.range.end > 9_007_199_254_740_991
                || (!request.relationships.facets.is_empty()
                    && !request.relationships.facets.contains(&occurrence.facet))
            {
                return Err(invalid(
                    "occurrence does not match the selected subject or source range",
                ));
            }
        }
        for record in &group.relationships {
            let relationship = &record.relationship;
            SymbolIdentity::parse(relationship.from.as_str())
                .map_err(|_| invalid("relationship endpoint is not canonical"))?;
            SymbolIdentity::parse(relationship.to.as_str())
                .map_err(|_| invalid("relationship endpoint is not canonical"))?;
            let matches = match request.relationships.direction {
                rift_protocol::read::TraversalDirection::Incoming => &relationship.to == seed,
                rift_protocol::read::TraversalDirection::Outgoing => &relationship.from == seed,
            };
            if !matches
                || (!request.relationships.facets.is_empty()
                    && !relationship
                        .facets
                        .iter()
                        .any(|facet| request.relationships.facets.contains(facet)))
            {
                return Err(invalid(
                    "relationship record does not match the selected subject, direction or facets",
                ));
            }
        }
    }
    if i64::try_from(count).map_or(true, |count| count > limit)
        || (page.next_cursor.is_none() && page.groups.iter().any(|group| !group.complete))
    {
        return Err(invalid(
            "relationship page did not establish bounded complete adjacency",
        ));
    }
    if request
        .view
        .as_ref()
        .is_some_and(|view| view != &page.view.id)
    {
        return Err(invalid("relationship page changed the selected view"));
    }
    if let Some(cursor) = &page.next_cursor {
        crate::validate_page(limit, Some(cursor))?;
    }
    let expiry = chrono::DateTime::parse_from_rfc3339(&page.view.expires_at)
        .map_err(|_| invalid("relationship view expiry is invalid"))?;
    if expiry <= chrono::Utc::now() {
        return Err(invalid(
            "relationship view expired before acquisition completed",
        ));
    }
    Ok(())
}

fn count_within(count: usize, maximum: u64) -> bool {
    u64::try_from(count).is_ok_and(|count| count <= maximum)
}

fn page_record_ids(page: &RelationshipSearchPage) -> Result<HashSet<&str>, RiftError> {
    let mut ids = HashSet::new();
    for record in page.groups.iter().flat_map(|group| &group.relationships) {
        if !ids.insert(record.id.as_str()) {
            return Err(invalid(
                "relationship record identity repeats within a page",
            ));
        }
    }
    let mut usages = std::collections::BTreeMap::new();
    for record in page.groups.iter().flat_map(|group| &group.occurrences) {
        if let Some(previous) = usages.insert(record.id.as_str(), record.relationship.as_ref())
            && previous != record.relationship.as_ref()
        {
            return Err(invalid(
                "original usage record changed its relationship link",
            ));
        }
    }
    ids.extend(usages.into_keys());
    Ok(ids)
}

fn merge_page(
    selected: &mut RelationshipSearchPage,
    page: RelationshipSearchPage,
) -> Result<(), RiftError> {
    if selected.view != page.view
        || selected.analyzer_revision != page.analyzer_revision
        || selected.corpus_revision != page.corpus_revision
    {
        return Err(invalid("relationship publication changed between pages"));
    }
    for (held, group) in selected.groups.iter_mut().zip(page.groups) {
        if held.complete
            && (!group.complete || !group.relationships.is_empty() || !group.occurrences.is_empty())
        {
            return Err(invalid(
                "completed relationship subject changed on a later page",
            ));
        }
        held.relationships.extend(group.relationships);
        held.occurrences.extend(group.occurrences);
        held.complete = group.complete;
    }
    selected.next_cursor = page.next_cursor;
    facts::merge(selected, page.symbols, page.sources)?;
    for warning in page.warnings {
        if !selected.warnings.contains(&warning) {
            selected.warnings.push(warning);
        }
    }
    if selected.warnings.len() > crate::WARNINGS_MAX {
        return Err(invalid("relationship warnings exceed the accepted bound"));
    }
    Ok(())
}

fn invalid(detail: &str) -> RiftError {
    failure::response_invalid("relationship_page", "relationships", detail, None)
}
