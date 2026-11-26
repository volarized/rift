//! Raw adjacency through the existing bounded global search transport.

use std::collections::HashSet;

use rift_error::RiftError;
use rift_protocol::edge_read::{
    EDGES_FEATURE, EdgeBounds, EdgeCapability, EdgeGroup, EdgeSearchPage, EdgeSearchRequest,
    EdgeSearchResult, EdgeSelection,
};
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::ReadWarning;

use crate::{GlobalClient, contract, failure, response};

mod facts;

/// Accepted operation bounds supplied by the shared query acquisition owner.
#[derive(Clone, Copy, Debug)]
pub struct EdgeReadLimits {
    /// Most HTTP pages acquired for this batch.
    pub requests_max: u64,
    /// Most original edge records retained across all pages.
    pub records_max: u64,
    /// Most serialized edge bytes retained across all pages.
    pub bytes_max: u64,
}

impl GlobalClient {
    /// Reads the exact direction and kinds supported by the advertised reader.
    /// Older metadata without this optional field establishes no edge operation.
    ///
    /// # Errors
    /// Returns invalid response metadata for duplicate directions, empty or repeated kinds.
    pub fn edge_capabilities(
        capabilities: &crate::Capabilities,
    ) -> Result<Vec<EdgeCapability>, RiftError> {
        let Some(operations) = capabilities.edge_capabilities.as_ref() else {
            return Ok(Vec::new());
        };
        let supported = operations
            .iter()
            .map(|operation| {
                serde_json::from_value::<EdgeCapability>(serde_json::json!({
                    "direction": operation.direction,
                    "kinds": operation.kinds,
                }))
                .map(|mut selected| {
                    selected.bounds = operation.bounds.as_ref().map(|bounds| EdgeBounds {
                        max_seeds: bounds.max_seeds,
                        max_kinds: bounds.max_kinds,
                        max_edges: bounds.max_edges,
                        max_occurrences: bounds.max_occurrences,
                        max_symbols: bounds.max_symbols,
                        max_sources: bounds.max_sources,
                    });
                    selected
                })
                .map_err(|_| invalid("edge capabilities are invalid"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if supported.len() > 2 {
            return Err(invalid("edge capabilities repeat a direction"));
        }
        if !supported.is_empty() && !crate::supports_feature(capabilities, EDGES_FEATURE) {
            return Err(invalid(
                "edge operation metadata differs from the advertised feature",
            ));
        }
        for (index, operation) in supported.iter().enumerate() {
            if operation.kinds.is_empty()
                || operation.bounds.as_ref().is_some_and(|bounds| {
                    [
                        bounds.max_seeds,
                        bounds.max_kinds,
                        bounds.max_edges,
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
                    .kinds
                    .iter()
                    .enumerate()
                    .any(|(position, kind)| operation.kinds[..position].contains(kind))
            {
                return Err(invalid(
                    "edge capabilities repeat or omit a required selection",
                ));
            }
        }
        Ok(supported)
    }

    /// Intersects the accepted local counts with the advertised raw operation bounds.
    ///
    /// # Errors
    /// Returns unsupported when the exact raw operation or its count policy is absent.
    pub fn edge_bounds(
        &self,
        capabilities: &crate::Capabilities,
        direction: rift_protocol::read::TraversalDirection,
        kinds: &[rift_protocol::read::EdgeKind],
    ) -> Result<EdgeBounds, RiftError> {
        let selected = Self::edge_capabilities(capabilities)?
            .into_iter()
            .find(|operation| {
                operation.direction == direction
                    && kinds.iter().all(|kind| operation.kinds.contains(kind))
            });
        let bounds = selected
            .and_then(|operation| operation.bounds)
            .ok_or_else(|| {
                rift_error::errors::server::read_unsupported()
                    .capability("global edge operation with advertised count bounds")
                    .error()
            })?;
        // A local usize above u64::MAX cannot restrict an advertised u64 count.
        let count = |remote: u64, local: usize| {
            u64::try_from(local).map_or(remote, |local| remote.min(local))
        };
        let config = &self.inner.config;
        Ok(EdgeBounds {
            max_seeds: count(bounds.max_seeds, config.max_seeds),
            max_kinds: count(bounds.max_kinds, config.max_kinds),
            max_edges: count(bounds.max_edges, config.max_edges),
            max_occurrences: count(bounds.max_occurrences, config.max_occurrences),
            max_symbols: count(bounds.max_symbols, config.max_symbols),
            max_sources: count(bounds.max_sources, config.max_sources),
        })
    }

    /// Reads one raw adjacency batch without ranking, endpoint deduplication or shortest paths.
    ///
    /// # Errors
    /// Returns request, capability, transport or strict response validation failure.
    pub async fn edge_page(
        &self,
        request: &EdgeSearchRequest,
        limit: i64,
    ) -> Result<EdgeSearchResult, RiftError> {
        validate_request(request, &self.inner.config)?;
        crate::validate_numbered_page(limit, 1)?;
        let capabilities = self.get_capabilities().await?;
        let bounds =
            self.edge_bounds(&capabilities, request.edges.direction, &request.edges.kinds)?;
        if !count_within(request.edges.seeds.len(), bounds.max_seeds)
            || !count_within(request.edges.kinds.len(), bounds.max_kinds)
        {
            return Err(failure::request_invalid(
                "edge_page",
                "edges",
                "edge selection exceeds its advertised count bound",
            ));
        }
        let packages = request
            .packages
            .len()
            .saturating_add(request.runtimes.len());
        crate::validate_read_bounds(packages, limit, &capabilities)?;
        // Larger counts cannot lower the accepted i64 transport page limit.
        let limit = limit
            .min(i64::try_from(bounds.max_edges).unwrap_or(i64::MAX))
            .min(i64::try_from(bounds.max_occurrences).unwrap_or(i64::MAX))
            .max(capabilities.bounds.page_limit_min);
        crate::validate_read_bounds(packages, limit, &capabilities)?;
        let body = serialize_request(
            request,
            crate::smaller_bound(
                capabilities.bounds.request_body_bytes_max,
                self.inner.config.max_request,
            ),
        )?;
        let query = rift_protocol::read::GlobalPageParams {
            page: 1,
            max_results: limit.cast_unsigned(),
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
                    (200, EdgeSearchResult::Page(_)) | (503, EdgeSearchResult::Unavailable(_))
                )
            },
        ) {
            Ok(parsed) => parsed,
            Err(error) => return self.observed(Err(error)).await,
        };
        if let EdgeSearchResult::Page(page) = &parsed.value {
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
                    .map_err(|_| invalid("edge symbol count exceeds the platform bound"))?,
                usize::try_from(bounds.max_sources)
                    .map_err(|_| invalid("edge source count exceeds the platform bound"))?,
            ) {
                return self.observed(Err(error)).await;
            }
        }
        Ok(parsed.value)
    }

    /// Acquires a batch under the original context and explicit work bounds.
    ///
    /// # Errors
    /// Returns unavailable or validation failure when the selected facts cannot be established.
    pub async fn edge_pages(
        &self,
        request: &EdgeSearchRequest,
        limits: EdgeReadLimits,
    ) -> Result<EdgeSearchResult, RiftError> {
        self.edge_pages_with_admission(request, limits, || Ok(()))
            .await
    }

    /// Acquires a batch after admission by the request's shared work owner.
    ///
    /// # Errors
    /// Returns admission, unavailable or strict response validation failure.
    pub async fn edge_pages_with_admission(
        &self,
        request: &EdgeSearchRequest,
        limits: EdgeReadLimits,
        mut admit: impl FnMut() -> Result<(), RiftError>,
    ) -> Result<EdgeSearchResult, RiftError> {
        validate_request(request, &self.inner.config)?;
        if limits.requests_max == 0 {
            return Err(invalid(
                "edge acquisition exceeds the accepted request bound",
            ));
        }
        let capabilities = self.get_capabilities().await?;
        let limit = crate::advertised_page_limit(crate::PAGE_LIMIT_MAX, &capabilities);
        admit()?;
        let result = self.edge_page(request, limit).await?;
        if let EdgeSearchResult::Page(page) = &result {
            let records = page_record_ids(page)?.len() as u64;
            if records > limits.records_max {
                return Err(invalid(
                    "edge acquisition exceeds the accepted record bound",
                ));
            }
            let mut bytes = 0;
            admit_page_bytes(page, &mut bytes, limits.bytes_max)?;
            facts::validate_complete(request, page)?;
        }
        Ok(result)
    }
}

fn serialize_request(request: &EdgeSearchRequest, limit: usize) -> Result<Vec<u8>, RiftError> {
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

fn admit_page_bytes(page: &EdgeSearchPage, bytes: &mut u64, maximum: u64) -> Result<(), RiftError> {
    let serialized = page
        .groups
        .iter()
        .flat_map(|group| {
            group
                .edges
                .iter()
                .map(serde_json::to_vec)
                .chain(group.occurrences.iter().map(serde_json::to_vec))
        })
        .chain(page.symbols.iter().map(serde_json::to_vec))
        .chain(page.sources.iter().map(serde_json::to_vec));
    for encoded in serialized {
        let encoded = encoded.map_err(|source| {
            failure::response_source(
                "edge_pages",
                "edges",
                "JSON serialization failed",
                None,
                source,
            )
        })?;
        *bytes = bytes
            .checked_add(
                u64::try_from(encoded.len())
                    .map_err(|_| invalid("edge byte count exceeds the platform bound"))?,
            )
            .ok_or_else(|| invalid("edge byte count exceeds the platform bound"))?;
        if *bytes > maximum {
            return Err(invalid(
                "edge acquisition exceeds the accepted record or byte bound",
            ));
        }
    }
    Ok(())
}

fn validate_request(request: &EdgeSearchRequest, config: &crate::Config) -> Result<(), RiftError> {
    let selection = &request.edges;
    if selection.seeds.is_empty()
        || selection.seeds.len() > config.max_seeds
        || selection.kinds.len() > config.max_kinds
        || (request.packages.is_empty() && request.runtimes.is_empty() && request.view.is_none())
        || request
            .packages
            .len()
            .saturating_add(request.runtimes.len())
            > crate::PACKAGES_MAX
    {
        return Err(failure::request_invalid(
            "edge_page",
            "edges",
            "raw adjacency requires bounded subjects and an exact package domain or retained view",
        ));
    }
    validate_runtimes(request)?;
    let mut seeds = HashSet::new();
    if selection
        .kinds
        .iter()
        .enumerate()
        .any(|(index, kind)| selection.kinds[..index].contains(kind))
    {
        return Err(failure::request_invalid(
            "edge_page",
            "edges.kinds",
            "kinds must be distinct",
        ));
    }
    for seed in &selection.seeds {
        let identity = SymbolIdentity::parse(seed.as_str()).map_err(|source| {
            failure::request_invalid_source(
                "edge_page",
                "edges.seeds",
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
                "edge_page",
                "edges.seeds",
                "subject must have package or runtime ownership",
            ));
        }
        if !seeds.insert(seed.as_str()) {
            return Err(failure::request_invalid(
                "edge_page",
                "edges.seeds",
                "subjects must be distinct",
            ));
        }
    }
    validate_packages(request)
}

fn validate_runtimes(request: &EdgeSearchRequest) -> Result<(), RiftError> {
    let mut runtimes = HashSet::new();
    for runtime in &request.runtimes {
        runtime.owner()?;
        if !runtimes.insert(&runtime.runtime) {
            return Err(failure::request_invalid(
                "edge_page",
                "runtimes",
                "exact runtimes must be distinct",
            ));
        }
    }
    Ok(())
}

fn validate_packages(request: &EdgeSearchRequest) -> Result<(), RiftError> {
    let mut packages = HashSet::new();
    for package in &request.packages {
        let owner = package.owner()?;
        for (value, maximum) in [
            (&package.manager, crate::PACKAGE_MANAGER_CHARS_MAX),
            (&package.registry, crate::PACKAGE_REGISTRY_CHARS_MAX),
            (&package.name, crate::PACKAGE_NAME_CHARS_MAX),
            (&package.version, crate::PACKAGE_VERSION_CHARS_MAX),
        ] {
            if !crate::within_characters(value, maximum) {
                return Err(failure::request_invalid(
                    "edge_page",
                    "packages",
                    "exact package identity exceeds the accepted bound",
                ));
            }
        }
        if !packages.insert(owner) {
            return Err(failure::request_invalid(
                "edge_page",
                "packages",
                "exact packages must be distinct",
            ));
        }
    }
    Ok(())
}

fn validate_page(
    request: &EdgeSearchRequest,
    page: &EdgeSearchPage,
    limit: i64,
    bounds: &EdgeBounds,
) -> Result<(), RiftError> {
    if page.groups.len() != request.edges.seeds.len()
        || page.analyzer_revision.is_empty()
        || page.analyzer_revision.len() > crate::LABEL_CHARS_MAX
        || page.corpus_revision.is_empty()
        || page.corpus_revision.len() > crate::LABEL_CHARS_MAX
        || page.warnings.len() > crate::WARNINGS_MAX
    {
        return Err(invalid("edge page metadata or requested groups differ"));
    }
    if page.warnings.iter().any(|warning| {
        matches!(
            warning,
            ReadWarning::ObjectUnavailable(_)
                | ReadWarning::GlobalPageWarning { .. }
                | ReadWarning::PackageAbsent { .. }
                | ReadWarning::ResultsTruncated { .. }
                | ReadWarning::SourceUnavailable { .. }
        )
    }) {
        return Err(invalid("edge page reports incomplete coverage"));
    }
    let count = page_record_ids(page)?.len();
    for (seed, group) in request.edges.seeds.iter().zip(&page.groups) {
        if seed != &group.seed {
            return Err(invalid(
                "edge page omitted or reordered a requested subject",
            ));
        }
        if !count_within(group.edges.len(), bounds.max_edges)
            || !count_within(group.occurrences.len(), bounds.max_occurrences)
        {
            return Err(invalid("edge projections exceed their page bound"));
        }
        validate_group(group, &request.edges, request.include)?;
    }
    let incomplete = page.groups.iter().any(|group| !group.complete);
    let coverage_warning = page
        .warnings
        .iter()
        .any(|warning| matches!(warning, ReadWarning::EdgeCoverageMissing { .. }));
    if i64::try_from(count).map_or(true, |count| count > limit)
        || incomplete != coverage_warning
        || (incomplete && count == 0)
    {
        return Err(invalid(
            "edge page did not establish bounded adjacency or retained witnesses",
        ));
    }
    if request
        .view
        .as_ref()
        .is_some_and(|view| view != &page.view.id)
    {
        return Err(invalid("edge page changed the selected view"));
    }
    let expiry = chrono::DateTime::parse_from_rfc3339(&page.view.expires_at)
        .map_err(|_| invalid("edge view expiry is invalid"))?;
    if expiry <= chrono::Utc::now() {
        return Err(invalid("edge view expired before acquisition completed"));
    }
    Ok(())
}

fn validate_group(
    group: &EdgeGroup,
    selection: &EdgeSelection,
    include: rift_protocol::edge_read::EdgeInclude,
) -> Result<(), RiftError> {
    let seed = &group.seed;
    for record in &group.occurrences {
        let occurrence = &record.occurrence;
        SymbolIdentity::parse(occurrence.target.as_str())?;
        if let Some(enclosing) = &occurrence.enclosing {
            SymbolIdentity::parse(enclosing.as_str())?;
        }
        rift_core::SourceUnitId::parse(occurrence.source.unit.as_str())?;
        let matches = match selection.direction {
            rift_protocol::read::TraversalDirection::Incoming => &occurrence.target == seed,
            rift_protocol::read::TraversalDirection::Outgoing => {
                occurrence.enclosing.as_ref() == Some(seed)
            }
        };
        if !matches
            || occurrence.source.range.start > occurrence.source.range.end
            || occurrence.source.range.end > 9_007_199_254_740_991
            || (!selection.kinds.is_empty() && !selection.kinds.contains(&occurrence.kind))
        {
            return Err(invalid(
                "occurrence does not match the selected subject or source range",
            ));
        }
    }
    let mut identities = std::collections::BTreeSet::new();
    for record in &group.edges {
        let edge = &record.edge;
        if !identities.insert(edge.identity()) {
            return Err(invalid("edge page repeats a merged edge"));
        }
        if include.providers == edge.providers.is_empty() {
            return Err(invalid(
                "edge providers differ from the requested projection",
            ));
        }
        SymbolIdentity::parse(edge.from.as_str())
            .map_err(|_| invalid("edge endpoint is not canonical"))?;
        SymbolIdentity::parse(edge.to.as_str())
            .map_err(|_| invalid("edge endpoint is not canonical"))?;
        let matches = match selection.direction {
            rift_protocol::read::TraversalDirection::Incoming => &edge.to == seed,
            rift_protocol::read::TraversalDirection::Outgoing => &edge.from == seed,
        };
        if !matches
            || edge.kinds.is_empty()
            || (!selection.kinds.is_empty()
                && !edge.kinds.iter().any(|kind| selection.kinds.contains(kind)))
        {
            return Err(invalid(
                "edge record does not match the selected subject, direction or kinds",
            ));
        }
    }
    Ok(())
}

fn count_within(count: usize, maximum: u64) -> bool {
    u64::try_from(count).is_ok_and(|count| count <= maximum)
}

fn page_record_ids(page: &EdgeSearchPage) -> Result<HashSet<&str>, RiftError> {
    let mut ids = HashSet::new();
    for record in page.groups.iter().flat_map(|group| &group.edges) {
        if !ids.insert(record.id.as_str()) {
            return Err(invalid("edge record identity repeats within a page"));
        }
    }
    let mut usages = std::collections::BTreeMap::new();
    for record in page.groups.iter().flat_map(|group| &group.occurrences) {
        if let Some(previous) = usages.insert(record.id.as_str(), record.edge.as_ref())
            && previous != record.edge.as_ref()
        {
            return Err(invalid("original usage record changed its edge link"));
        }
    }
    ids.extend(usages.into_keys());
    Ok(ids)
}

fn invalid(detail: &str) -> RiftError {
    failure::response_invalid("edge_page", "edges", detail, None)
}
