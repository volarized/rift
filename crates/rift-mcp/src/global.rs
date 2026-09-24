//! Global package routing for current-tree reads.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex};

use percent_encoding::percent_decode_str;
use rift_cloud_client::{
    ClientError, Config, ConfigError, GlobalClient, PackageAvailability as WireAvailability,
    PackageContextEntry as WireContextEntry, PackageIdentity as WirePackageIdentity,
    PackageResolutionRequest, PackageSearchCandidate, PackageSearchRequest,
    PackageSearchRequestPhase, PackageSearchRequestTarget, PackageSymbolCandidate,
    PackageSymbolRequest, PackageSymbolRequestInclude, QueryTerm, Warning, WarningCode,
};
use rift_dependency::DependencyContext;
use rift_protocol::configuration::GlobalConfiguration;
use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry};
use rift_protocol::read::{
    DEPENDENCY_WARNINGS_MAX, GetSymbolInclude, GetSymbolParams, GetSymbolResult,
    GlobalFailureClass, GlobalPageWarningCode, PackageIdentity, Pagination, ReadWarning,
    ResultOrder, SearchHit, SearchHitTarget, SearchInclude, SearchParams, SearchResult,
    SearchScope,
};
use rift_ranking::{
    DocumentIdentity, FieldSet, ParsedQuery, QueryPhase, RankedIdentity, RankingInput,
    RankingInputKind, RankingWeights, SearchableField, fuse, match_class,
};
use tokio::sync::Mutex;

/// Most package-manager summaries one read emits. Remaining managers are not logged.
const FALLBACK_LOG_MANAGERS_MAX: usize = 8;
/// Maximum UTF-8 bytes retained for one package-manager log label.
const FALLBACK_LOG_MANAGER_BYTES_MAX: usize = 64;

/// One client shared by reads under the same accepted configuration and credential value.
#[derive(Default)]
pub(crate) struct GlobalState {
    client: Mutex<Option<ClientSlot>>,
    observation: Arc<StdMutex<Option<ServiceState>>>,
}

impl fmt::Debug for GlobalState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GlobalState")
            .finish_non_exhaustive()
    }
}

struct ClientSlot {
    config: Config,
    credential: Option<OsString>,
    client: GlobalClient,
}

/// Global selection made before one local package read.
pub(crate) struct GlobalRoute {
    pub(crate) client: Option<GlobalClient>,
    pub(crate) remote_packages: Vec<WirePackageIdentity>,
    pub(crate) fallback_context: Arc<DependencyContext>,
    pub(crate) missing_exact: Vec<PackageIdentity>,
    pub(crate) missing_requirements: Vec<PackageContextEntry>,
    pub(crate) state: RouteState,
    observation: Arc<StdMutex<Option<ServiceState>>>,
}

/// Why one read selected its local package fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteState {
    Disabled,
    Available,
    Unavailable {
        kind: FailureKind,
        class: GlobalFailureClass,
    },
}

/// Protocol warning family for one bounded client failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FailureKind {
    Api,
    Publication,
    Response,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceState {
    NotConfigured,
    Unavailable,
    Available,
}

impl GlobalState {
    /// Resolves canonical package entries and selects entries requiring local fallback.
    pub(crate) async fn route(
        &self,
        configuration: &GlobalConfiguration,
        context: &Arc<DependencyContext>,
    ) -> GlobalRoute {
        let route = self.route_inner(configuration, context).await;
        route.record_observation();
        route
    }

    /// Selects local fallback when the enclosing MCP request deadline expires.
    pub(crate) fn deadline_exceeded(&self, context: &Arc<DependencyContext>) -> GlobalRoute {
        let route = GlobalRoute::fallback(
            context,
            failure_state(&ClientError::Deadline),
            Arc::clone(&self.observation),
        );
        route.record_observation();
        route
    }

    async fn route_inner(
        &self,
        configuration: &GlobalConfiguration,
        context: &Arc<DependencyContext>,
    ) -> GlobalRoute {
        if !configuration.enabled {
            return GlobalRoute::fallback(
                context,
                RouteState::Disabled,
                Arc::clone(&self.observation),
            );
        }
        let request = resolution_request(context);
        if request.entries.is_empty() {
            return GlobalRoute {
                client: None,
                remote_packages: Vec::new(),
                fallback_context: Arc::new(
                    context.filter_entries(|entry| {
                        entry.availability == PackageAvailability::LocalOnly
                    }),
                ),
                missing_exact: Vec::new(),
                missing_requirements: Vec::new(),
                state: RouteState::Available,
                observation: Arc::clone(&self.observation),
            };
        }
        let client = match self.client(configuration).await {
            Ok(client) => client,
            Err(error) => {
                return GlobalRoute::fallback(
                    context,
                    failure_state(&ClientError::Config(error)),
                    Arc::clone(&self.observation),
                );
            }
        };
        match client.resolve_package_context(&request).await {
            Ok(resolution) => {
                resolved_route(context, client, resolution, Arc::clone(&self.observation))
            }
            Err(error) => GlobalRoute::fallback(
                context,
                failure_state(&error),
                Arc::clone(&self.observation),
            ),
        }
    }

    async fn client(
        &self,
        configuration: &GlobalConfiguration,
    ) -> Result<GlobalClient, ConfigError> {
        let config = Config::try_from(configuration)?;
        let credential = std::env::var_os(&config.token_env);
        let mut held = self.client.lock().await;
        if let Some(slot) = held.as_ref()
            && slot.config == config
            && slot.credential == credential
        {
            return Ok(slot.client.clone());
        }
        let client = GlobalClient::new(config.clone())?;
        *held = Some(ClientSlot {
            config,
            credential,
            client: client.clone(),
        });
        Ok(client)
    }
}

/// Reads package declarations for one accepted symbol request.
pub(crate) async fn package_symbols(
    client: &GlobalClient,
    params: &GetSymbolParams,
    packages: &[WirePackageIdentity],
) -> Result<GlobalSymbolCandidates, ClientError> {
    let request = PackageSymbolRequest {
        name: params.name.clone(),
        language: params
            .language
            .as_ref()
            .map(rift_core::Language::identity_segment),
        include: symbol_include(params),
        packages: packages.to_vec(),
    };
    let page = client
        .list_package_symbols_pages(
            &request,
            i64::try_from(params.limit.min(rift_cloud_client::PAGE_LIMIT_MAX as u64))
                .unwrap_or(rift_cloud_client::PAGE_LIMIT_MAX),
        )
        .await?;
    let warnings = page_warnings(page.warnings);
    let items = page
        .items
        .into_iter()
        .map(PackageSymbolCandidate::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(GlobalSymbolCandidates { items, warnings })
}

/// Package symbol candidates and page warnings returned by one remote read.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GlobalSymbolCandidates {
    pub(crate) items: Vec<PackageSymbolCandidate>,
    pub(crate) warnings: Vec<ReadWarning>,
}

/// Reads precise and broad package search phases in order.
pub(crate) async fn package_search(
    client: &GlobalClient,
    params: &SearchParams,
    query: &ParsedQuery,
    packages: &[WirePackageIdentity],
) -> Result<GlobalSearchCandidates, ClientError> {
    if params.target == rift_protocol::read::SearchParamsTarget::File {
        return Ok(GlobalSearchCandidates::default());
    }
    let page_limit = rift_cloud_client::PAGE_LIMIT_MAX;
    let precise_page = client
        .search_packages_pages(
            &search_request(params, query, packages, QueryPhase::Precise),
            page_limit,
        )
        .await?;
    let mut warnings = page_warnings(precise_page.warnings);
    let precise = precise_page
        .items
        .into_iter()
        .map(PackageSearchCandidate::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    let broad = if query.has_broad_phase() && precise.len() < rift_cloud_client::CANDIDATE_POOL_MAX
    {
        let broad_page = client
            .search_packages_pages(
                &search_request(params, query, packages, QueryPhase::Broad),
                page_limit,
            )
            .await?;
        extend_page_warnings(&mut warnings, broad_page.warnings);
        broad_page
            .items
            .into_iter()
            .map(PackageSearchCandidate::try_from)
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    Ok(GlobalSearchCandidates {
        precise,
        broad,
        warnings,
    })
}

/// Package search candidates grouped by ranking phase.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GlobalSearchCandidates {
    pub(crate) precise: Vec<PackageSearchCandidate>,
    pub(crate) broad: Vec<PackageSearchCandidate>,
    pub(crate) warnings: Vec<ReadWarning>,
}

fn page_warnings(warnings: Vec<Warning>) -> Vec<ReadWarning> {
    let mut mapped = Vec::new();
    extend_page_warnings(&mut mapped, warnings);
    mapped
}

fn extend_page_warnings(target: &mut Vec<ReadWarning>, warnings: Vec<Warning>) {
    for warning in warnings {
        if target.len() == rift_cloud_client::WARNINGS_MAX {
            break;
        }
        let warning = ReadWarning::GlobalPageWarning {
            warning_code: match warning.code {
                WarningCode::QueryNarrowed => GlobalPageWarningCode::QueryNarrowed,
                WarningCode::SourceTruncated => GlobalPageWarningCode::SourceTruncated,
                WarningCode::PublicationChanged => GlobalPageWarningCode::PublicationChanged,
                WarningCode::CapabilityUnavailable => GlobalPageWarningCode::CapabilityUnavailable,
                WarningCode::ResultTruncated => GlobalPageWarningCode::ResultTruncated,
                WarningCode::Unknown => GlobalPageWarningCode::Unknown,
            },
            detail: warning.detail,
        };
        if !target.contains(&warning) {
            target.push(warning);
        }
    }
}

fn symbol_include(params: &GetSymbolParams) -> Option<Vec<PackageSymbolRequestInclude>> {
    let mut fields = Vec::new();
    if params.include.contains(&GetSymbolInclude::Source) {
        fields.push(PackageSymbolRequestInclude::Source);
    }
    if params.include.contains(&GetSymbolInclude::Documentation) {
        fields.push(PackageSymbolRequestInclude::Documentation);
    }
    (!fields.is_empty()).then_some(fields)
}

fn search_request(
    params: &SearchParams,
    query: &ParsedQuery,
    packages: &[WirePackageIdentity],
    phase: QueryPhase,
) -> PackageSearchRequest {
    PackageSearchRequest {
        query: query.source().to_owned(),
        terms: query
            .members()
            .iter()
            .map(|member| QueryTerm {
                text: member.text().to_owned(),
                phrase: member.is_phrase(),
                prefix: phase == QueryPhase::Broad && !member.is_phrase(),
            })
            .collect(),
        identifiers: query
            .candidates()
            .into_iter()
            .map(|candidate| candidate.text().to_owned())
            .collect(),
        include: params
            .include
            .as_deref()
            .unwrap_or_default()
            .contains(&SearchInclude::Source)
            .then(|| vec!["source".to_owned()]),
        packages: packages.to_vec(),
        phase: match phase {
            QueryPhase::Precise => PackageSearchRequestPhase::Precise,
            QueryPhase::Broad => PackageSearchRequestPhase::Broad,
        },
        target: match params.target {
            rift_protocol::read::SearchParamsTarget::Symbol
            | rift_protocol::read::SearchParamsTarget::File => None,
            rift_protocol::read::SearchParamsTarget::Documentation => {
                Some(PackageSearchRequestTarget::Documentation)
            }
            rift_protocol::read::SearchParamsTarget::All => Some(PackageSearchRequestTarget::All),
        },
    }
}

/// Merges local and remote symbol hits, then applies requested pagination.
pub(crate) fn merge_symbols(
    params: &GetSymbolParams,
    mut local: GetSymbolResult,
    remote: Vec<PackageSymbolCandidate>,
) -> Result<GetSymbolResult, ClientError> {
    let mut entries = Vec::with_capacity(local.hits.len() + remote.len());
    for hit in local.hits.drain(..) {
        let symbol_id = hit
            .symbol
            .id
            .clone()
            .ok_or(ClientError::InvalidResponseField {
                field: "symbol_identity",
            })?;
        let package = hit.symbol.origin.package.clone();
        let class = local_match_class(&params.name, &hit)?;
        entries.push(SymbolEntry {
            hit,
            symbol_id,
            package,
            class,
            remote: false,
        });
    }
    entries.extend(remote.into_iter().map(|candidate| SymbolEntry {
        symbol_id: candidate.symbol_identity.clone(),
        package: Some(candidate.package.clone()),
        class: candidate.match_class,
        hit: candidate.hit,
        remote: true,
    }));
    entries.sort_by(|left, right| symbol_order(left, right, params.scope));
    entries.dedup_by(|left, right| left.symbol_id == right.symbol_id);
    let hits = entries
        .into_iter()
        .map(|entry| entry.hit)
        .collect::<Vec<_>>();
    let page_limit = usize::try_from(params.limit)
        .map_err(|_| ClientError::InvalidRequest { field: "limit" })?;
    let (hits, pagination) = page_window(hits, params.page_index, page_limit);
    Ok(GetSymbolResult {
        hits,
        pagination,
        warnings: local.warnings,
    })
}

#[derive(Debug)]
struct SymbolEntry {
    hit: rift_protocol::read::GetSymbolHit,
    symbol_id: rift_protocol::read::SymbolId,
    package: Option<PackageIdentity>,
    class: rift_ranking::IdentifierMatchClass,
    remote: bool,
}

fn local_match_class(
    query: &str,
    hit: &rift_protocol::read::GetSymbolHit,
) -> Result<rift_ranking::IdentifierMatchClass, ClientError> {
    let qualified_name = hit
        .symbol
        .id
        .as_ref()
        .and_then(|id| id.0.rsplit('/').next())
        .ok_or(ClientError::InvalidResponseField {
            field: "symbol_identity",
        })?;
    let qualified_name = percent_decode_str(qualified_name)
        .decode_utf8()
        .map_err(|_| ClientError::InvalidResponseField {
            field: "symbol_identity",
        })?;
    match_class(
        &query.to_lowercase(),
        &hit.symbol.name.to_lowercase(),
        &qualified_name.to_lowercase(),
    )
    .ok_or(ClientError::InvalidResponseField {
        field: "symbol_match",
    })
}

fn symbol_order(left: &SymbolEntry, right: &SymbolEntry, scope: SearchScope) -> std::cmp::Ordering {
    left.class
        .cmp(&right.class)
        .then_with(|| {
            if scope == SearchScope::All {
                left.package.is_some().cmp(&right.package.is_some())
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .then_with(|| package_key(left.package.as_ref()).cmp(&package_key(right.package.as_ref())))
        .then_with(|| left.remote.cmp(&right.remote))
        .then_with(|| left.symbol_id.0.cmp(&right.symbol_id.0))
}

fn package_key(package: Option<&PackageIdentity>) -> (String, String, String) {
    package
        .map(|package| {
            (
                package.manager.clone(),
                package.name.clone(),
                package.version.clone(),
            )
        })
        .unwrap_or_default()
}

/// Merges package candidates into local search hits while retaining traversal-only hits.
pub(crate) fn merge_search(
    params: &SearchParams,
    mut local: SearchResult,
    remote: GlobalSearchCandidates,
) -> Result<SearchResult, ClientError> {
    let mut payloads = std::collections::BTreeMap::new();
    let mut local_order = Vec::new();
    for hit in local.results.drain(..) {
        let identity = search_identity(&hit)?;
        local_order.push(identity.clone());
        payloads.insert(identity, hit);
    }
    let mut precise = Vec::new();
    for candidate in remote.precise {
        payloads
            .entry(candidate.identity.clone())
            .or_insert_with(|| candidate.hit.clone());
        precise.push(candidate);
    }
    let mut broad = Vec::new();
    for candidate in remote.broad {
        payloads
            .entry(candidate.identity.clone())
            .or_insert_with(|| candidate.hit.clone());
        broad.push(candidate);
    }
    let local_input = RankingInput::new(
        RankingInputKind::Identifier,
        local_order
            .into_iter()
            .map(|identity| RankedIdentity::new(identity, FieldSet::of(SearchableField::Name)))
            .collect(),
    );
    let remote_input = RankingInput::new(
        RankingInputKind::Lexical,
        precise
            .iter()
            .map(|candidate| {
                RankedIdentity::new(candidate.identity.clone(), candidate_fields(&candidate.hit))
            })
            .collect(),
    );
    let weights =
        RankingWeights::new(0.5, 0.5, 0.0, 60).map_err(|_| ClientError::InvalidResponseField {
            field: "ranking_weights",
        })?;
    let keep_max = payloads.len().max(1);
    let mut ranked = fuse(
        &[local_input, remote_input],
        weights,
        QueryPhase::Precise,
        keep_max,
    );
    if ranked.len() < keep_max {
        let broad_input = RankingInput::new(
            RankingInputKind::Lexical,
            broad
                .iter()
                .map(|candidate| {
                    RankedIdentity::new(
                        candidate.identity.clone(),
                        candidate_fields(&candidate.hit),
                    )
                })
                .collect(),
        );
        let broad_ranked = fuse(&[broad_input], weights, QueryPhase::Broad, keep_max);
        ranked.append_phase(broad_ranked, keep_max);
    }
    let include_score = params
        .include
        .as_deref()
        .unwrap_or_default()
        .contains(&SearchInclude::Score);
    let mut ordered = ranked
        .candidates()
        .iter()
        .filter_map(|candidate| {
            payloads.get(candidate.identity()).cloned().map(|mut hit| {
                hit.score = include_score.then(|| candidate.score());
                hit
            })
        })
        .collect::<Vec<_>>();
    order_search_hits(&mut ordered, params.order);
    let limit = usize::try_from(
        params
            .limit
            .unwrap_or(rift_core::constants::SEARCH_RESULTS_DEFAULT as u64),
    )
    .unwrap_or(usize::MAX);
    let (results, pagination) = page_window(ordered, params.page_index, limit);
    Ok(SearchResult {
        results,
        pagination,
        warnings: local.warnings,
    })
}

fn candidate_fields(hit: &SearchHit) -> FieldSet {
    let fields: FieldSet = hit
        .matched_by
        .iter()
        .filter_map(|field| match field {
            rift_protocol::read::MatchedField::Name => Some(SearchableField::Name),
            rift_protocol::read::MatchedField::Signature => Some(SearchableField::Signature),
            rift_protocol::read::MatchedField::Documentation => {
                Some(SearchableField::Documentation)
            }
            rift_protocol::read::MatchedField::Content => Some(SearchableField::DeclarationSource),
            _ => None,
        })
        .collect();
    if fields.is_empty() {
        FieldSet::of(SearchableField::QualifiedName)
    } else {
        fields
    }
}

fn search_identity(hit: &SearchHit) -> Result<DocumentIdentity, ClientError> {
    let identity =
        match &hit.hit {
            SearchHitTarget::Symbol { symbol } => {
                let symbol_id = symbol
                    .id
                    .as_ref()
                    .ok_or(ClientError::InvalidResponseField {
                        field: "symbol_identity",
                    })?;
                if let Some(unit) = hit.unit.as_ref() {
                    let encoded = symbol_id.0.rsplit('/').next().ok_or(
                        ClientError::InvalidResponseField {
                            field: "symbol_identity",
                        },
                    )?;
                    let qualified_name =
                        percent_decode_str(encoded).decode_utf8().map_err(|_| {
                            ClientError::InvalidResponseField {
                                field: "symbol_identity",
                            }
                        })?;
                    DocumentIdentity::for_unit(
                        &rift_core::SourceUnitId::parse(&unit.0).map_err(|_| {
                            ClientError::InvalidResponseField {
                                field: "source_identity",
                            }
                        })?,
                        &qualified_name,
                    )
                } else {
                    DocumentIdentity::new(symbol_id.0.clone())
                }
            }
            SearchHitTarget::File { .. } => DocumentIdentity::new(
                hit.path
                    .as_ref()
                    .ok_or(ClientError::InvalidResponseField {
                        field: "file_identity",
                    })?
                    .0
                    .clone(),
            ),
            SearchHitTarget::Node { node } => DocumentIdentity::new(node.0.clone()),
            SearchHitTarget::Documentation { documentation } => {
                DocumentIdentity::for_documentation_block(&documentation.block.identity.0)
            }
        };
    identity.map_err(|_| ClientError::InvalidResponseField {
        field: "search_identity",
    })
}

fn order_search_hits(hits: &mut [SearchHit], order: ResultOrder) {
    match order {
        ResultOrder::Relevance => {}
        ResultOrder::Path => hits.sort_by(|left, right| {
            left.path
                .is_none()
                .cmp(&right.path.is_none())
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.unit.cmp(&right.unit))
                .then_with(|| search_hit_key(left).cmp(search_hit_key(right)))
        }),
        ResultOrder::Identity => {
            hits.sort_by(|left, right| search_hit_key(left).cmp(search_hit_key(right)));
        }
    }
}

fn search_hit_key(hit: &SearchHit) -> &str {
    match &hit.hit {
        SearchHitTarget::Symbol { symbol } => symbol
            .id
            .as_ref()
            .map_or(symbol.name.as_str(), |identity| identity.0.as_str()),
        SearchHitTarget::File { .. } => hit.path.as_ref().map_or("", |path| path.0.as_str()),
        SearchHitTarget::Node { node } => node.0.as_str(),
        SearchHitTarget::Documentation { documentation } => documentation.block.identity.0.as_str(),
    }
}

fn page_window<T>(items: Vec<T>, page_index: u64, limit: usize) -> (Vec<T>, Pagination) {
    let total_pages = u64::try_from(items.len().div_ceil(limit)).unwrap_or(u64::MAX);
    let start = usize::try_from(page_index)
        .ok()
        .and_then(|page| page.checked_mul(limit))
        .unwrap_or(usize::MAX);
    let page = if start >= items.len() {
        Vec::new()
    } else {
        items.into_iter().skip(start).take(limit).collect()
    };
    (
        page,
        Pagination {
            page_index,
            total_pages,
        },
    )
}

impl GlobalRoute {
    /// Discards a failed remote lane and selects the complete dependency context locally.
    pub(crate) fn discard_remote(&mut self, context: &Arc<DependencyContext>, error: &ClientError) {
        self.client = None;
        self.remote_packages.clear();
        self.fallback_context = Arc::clone(context);
        self.missing_exact.clear();
        self.missing_requirements.clear();
        self.state = failure_state(error);
        self.record_observation();
    }

    /// Typed warning for this route's local fallback, when global access did not answer.
    pub(crate) fn fallback_warning(&self, fallback_indexed: u64) -> Option<ReadWarning> {
        let selected = bounded_count(self.fallback_context.entries().len());
        let fallback_indexed = fallback_indexed.min(selected);
        let fallback_unresolved = selected.saturating_sub(fallback_indexed);
        match self.state {
            RouteState::Disabled => Some(ReadWarning::GlobalAccessDisabled {
                fallback_indexed,
                fallback_unresolved,
            }),
            RouteState::Available => None,
            RouteState::Unavailable { kind, class } => Some(match kind {
                FailureKind::Api => ReadWarning::GlobalApiUnavailable {
                    failure_class: class,
                    fallback_indexed,
                    fallback_unresolved,
                },
                FailureKind::Publication => ReadWarning::GlobalPublicationIncompatible {
                    failure_class: class,
                    fallback_indexed,
                    fallback_unresolved,
                },
                FailureKind::Response => ReadWarning::GlobalResponseInvalid {
                    failure_class: class,
                    fallback_indexed,
                    fallback_unresolved,
                },
            }),
        }
    }

    /// Packages a valid global resolution selected for local fallback.
    pub(crate) fn missing_warnings(&self) -> impl Iterator<Item = ReadWarning> + '_ {
        self.missing_exact
            .iter()
            .cloned()
            .map(|package| ReadWarning::PackageAbsent { package })
            .chain(
                self.missing_requirements
                    .iter()
                    .cloned()
                    .map(|entry| ReadWarning::PackageRequirementAbsent { entry }),
            )
            .take(DEPENDENCY_WARNINGS_MAX)
    }

    fn fallback(
        context: &Arc<DependencyContext>,
        state: RouteState,
        observation: Arc<StdMutex<Option<ServiceState>>>,
    ) -> Self {
        Self {
            client: None,
            remote_packages: Vec::new(),
            fallback_context: Arc::clone(context),
            missing_exact: Vec::new(),
            missing_requirements: Vec::new(),
            state,
            observation,
        }
    }

    fn record_observation(&self) {
        let service_state = self.state.service_state();
        let transitioned = {
            let mut observed = match self.observation.lock() {
                Ok(observed) => observed,
                Err(poisoned) => poisoned.into_inner(),
            };
            if *observed == Some(service_state) {
                false
            } else {
                *observed = Some(service_state);
                true
            }
        };
        if transitioned {
            log_service_transition(self.state);
        }
    }

    /// Records the result of one selected local fallback.
    pub(crate) fn record_fallback(&self, fallback_indexed: u64) {
        log_fallback_summary(self, fallback_indexed);
    }
}

impl RouteState {
    fn service_state(self) -> ServiceState {
        match self {
            Self::Disabled => ServiceState::NotConfigured,
            Self::Available => ServiceState::Available,
            Self::Unavailable { .. } => ServiceState::Unavailable,
        }
    }
}

fn log_service_transition(state: RouteState) {
    match state {
        RouteState::Disabled => tracing::info!(
            component = "global",
            operation = "global.state",
            state = "not_configured",
            "global service state changed"
        ),
        RouteState::Available => tracing::info!(
            component = "global",
            operation = "global.state",
            state = "available",
            "global service state changed"
        ),
        RouteState::Unavailable { class, .. } => tracing::warn!(
            component = "global",
            operation = "global.state",
            state = "unavailable",
            failure_class = failure_class_label(class),
            "global service state changed"
        ),
    }
}

fn log_fallback_summary(route: &GlobalRoute, fallback_indexed: u64) {
    if route.fallback_context.entries().is_empty() {
        return;
    }
    let selected_count = bounded_count(route.fallback_context.entries().len());
    let fallback_indexed = fallback_indexed.min(selected_count);
    let fallback_unresolved = selected_count.saturating_sub(fallback_indexed);
    let mut counts = BTreeMap::<&str, u64>::new();
    for entry in route.fallback_context.entries() {
        let count = counts.entry(entry.manager.as_str()).or_default();
        *count = count.saturating_add(1);
    }
    let manager_count = u64::try_from(counts.len()).unwrap_or(u64::MAX);
    let omitted_count = manager_count.saturating_sub(FALLBACK_LOG_MANAGERS_MAX as u64);
    let missing_count = bounded_count(
        route
            .missing_exact
            .len()
            .saturating_add(route.missing_requirements.len()),
    );
    for (manager, count) in counts.into_iter().take(FALLBACK_LOG_MANAGERS_MAX) {
        log_fallback_manager(
            route.state,
            bounded_manager(manager),
            FallbackLogCounts {
                selected: count.min(global_fallback_packages_max()),
                missing: missing_count,
                indexed: fallback_indexed,
                unresolved: fallback_unresolved,
                managers: manager_count.min(global_fallback_packages_max()),
                omitted: omitted_count.min(global_fallback_packages_max()),
            },
        );
    }
}

#[derive(Clone, Copy)]
struct FallbackLogCounts {
    selected: u64,
    missing: u64,
    indexed: u64,
    unresolved: u64,
    managers: u64,
    omitted: u64,
}

fn log_fallback_manager(state: RouteState, manager: &str, counts: FallbackLogCounts) {
    let FallbackLogCounts {
        selected,
        missing,
        indexed,
        unresolved,
        managers,
        omitted,
    } = counts;
    let fallback_outcome = fallback_outcome(indexed, unresolved);
    match state {
        RouteState::Disabled => tracing::info!(
            component = "global",
            operation = "global.fallback",
            state = "not_configured",
            manager,
            selected_count = selected,
            missing_count = missing,
            fallback_indexed = indexed,
            fallback_unresolved = unresolved,
            manager_count = managers,
            omitted_count = omitted,
            fallback_outcome,
            "global package fallback selected"
        ),
        RouteState::Available => tracing::info!(
            component = "global",
            operation = "global.fallback",
            state = "available",
            manager,
            selected_count = selected,
            missing_count = missing,
            fallback_indexed = indexed,
            fallback_unresolved = unresolved,
            manager_count = managers,
            omitted_count = omitted,
            fallback_outcome,
            "global package fallback selected"
        ),
        RouteState::Unavailable { class, .. } => tracing::warn!(
            component = "global",
            operation = "global.fallback",
            state = "unavailable",
            failure_class = failure_class_label(class),
            manager,
            selected_count = selected,
            missing_count = missing,
            fallback_indexed = indexed,
            fallback_unresolved = unresolved,
            manager_count = managers,
            omitted_count = omitted,
            fallback_outcome,
            "global package fallback selected"
        ),
    }
}

const fn fallback_outcome(fallback_indexed: u64, fallback_unresolved: u64) -> &'static str {
    if fallback_unresolved == 0 {
        "complete"
    } else if fallback_indexed == 0 {
        "unavailable"
    } else {
        "partial"
    }
}

fn bounded_count(count: usize) -> u64 {
    u64::try_from(count)
        .unwrap_or(u64::MAX)
        .min(global_fallback_packages_max())
}

fn global_fallback_packages_max() -> u64 {
    rift_protocol::read::GLOBAL_FALLBACK_PACKAGES_MAX
}

fn bounded_manager(manager: &str) -> &str {
    if manager.len() <= FALLBACK_LOG_MANAGER_BYTES_MAX {
        return manager;
    }
    let end = manager
        .char_indices()
        .find(|(index, _)| *index >= FALLBACK_LOG_MANAGER_BYTES_MAX)
        .map_or(manager.len(), |(index, _)| index);
    &manager[..end]
}

fn failure_class_label(class: GlobalFailureClass) -> &'static str {
    match class {
        GlobalFailureClass::Connection => "connection",
        GlobalFailureClass::Timeout => "timeout",
        GlobalFailureClass::RetryExhausted => "retry_exhausted",
        GlobalFailureClass::Authentication => "authentication",
        GlobalFailureClass::CredentialConfiguration => "credential_configuration",
        GlobalFailureClass::NonSuccessResponse => "non_success_response",
        GlobalFailureClass::InvalidResponse => "invalid_response",
        GlobalFailureClass::ResponseTruncated => "response_truncated",
        GlobalFailureClass::PublicationFormat => "publication_format",
        GlobalFailureClass::CorpusRevision => "corpus_revision",
        GlobalFailureClass::RequiredFieldSet => "required_field_set",
    }
}

fn resolution_request(context: &DependencyContext) -> PackageResolutionRequest {
    PackageResolutionRequest {
        entries: context
            .entries()
            .iter()
            .filter(|entry| entry.availability == PackageAvailability::Canonical)
            .map(wire_context_entry)
            .collect(),
    }
}

fn wire_context_entry(entry: &PackageContextEntry) -> WireContextEntry {
    WireContextEntry {
        availability: WireAvailability::Canonical,
        manager: entry.manager.clone(),
        name: entry.name.clone(),
        requirement: entry.requirement.clone(),
        version: entry.version.clone(),
    }
}

fn protocol_context_entry(entry: WireContextEntry) -> PackageContextEntry {
    PackageContextEntry {
        manager: entry.manager,
        name: entry.name,
        version: entry.version,
        requirement: entry.requirement,
        availability: match entry.availability {
            WireAvailability::Canonical => PackageAvailability::Canonical,
            WireAvailability::LocalOnly => PackageAvailability::LocalOnly,
        },
    }
}

fn resolved_route(
    context: &Arc<DependencyContext>,
    client: GlobalClient,
    resolution: rift_cloud_client::PackageResolutionResponse,
    observation: Arc<StdMutex<Option<ServiceState>>>,
) -> GlobalRoute {
    let mut served = HashSet::new();
    let mut remote_packages = Vec::new();
    for package in resolution.available_exact {
        served.insert(EntryKey::exact(
            &package.manager,
            &package.name,
            &package.version,
        ));
        push_distinct_package(&mut remote_packages, package);
    }
    for resolved in resolution.resolved_requirements {
        served.insert(EntryKey::from_wire(&resolved.entry));
        push_distinct_package(&mut remote_packages, resolved.package);
    }
    let missing_exact = resolution
        .missing_exact
        .into_iter()
        .map(protocol_package_identity)
        .collect();
    let missing_requirements = resolution
        .missing_requirements
        .into_iter()
        .map(protocol_context_entry)
        .collect();
    GlobalRoute {
        client: Some(client),
        remote_packages,
        fallback_context: Arc::new(context.filter_entries(|entry| {
            entry.availability == PackageAvailability::LocalOnly
                || !served.contains(&EntryKey::from_protocol(entry))
        })),
        missing_exact,
        missing_requirements,
        state: RouteState::Available,
        observation,
    }
}

fn push_distinct_package(packages: &mut Vec<WirePackageIdentity>, candidate: WirePackageIdentity) {
    if packages.iter().any(|held| {
        held.manager == candidate.manager
            && held.name == candidate.name
            && held.version == candidate.version
    }) {
        return;
    }
    packages.push(candidate);
}

fn protocol_package_identity(package: WirePackageIdentity) -> PackageIdentity {
    PackageIdentity {
        manager: package.manager,
        name: package.name,
        version: package.version,
    }
}

#[derive(Hash, Eq, PartialEq)]
struct EntryKey {
    manager: String,
    name: String,
    selector: String,
    exact: bool,
}

impl EntryKey {
    fn from_protocol(entry: &PackageContextEntry) -> Self {
        match (&entry.version, &entry.requirement) {
            (Some(version), None) => Self::exact(&entry.manager, &entry.name, version),
            (None, Some(requirement)) => {
                Self::requirement(&entry.manager, &entry.name, requirement)
            }
            _ => unreachable!("accepted dependency context carries exactly one selector"),
        }
    }

    fn from_wire(entry: &WireContextEntry) -> Self {
        match (&entry.version, &entry.requirement) {
            (Some(version), None) => Self::exact(&entry.manager, &entry.name, version),
            (None, Some(requirement)) => {
                Self::requirement(&entry.manager, &entry.name, requirement)
            }
            _ => unreachable!("validated global response carries exactly one selector"),
        }
    }

    fn exact(manager: &str, name: &str, version: &str) -> Self {
        Self {
            manager: manager.to_owned(),
            name: name.to_owned(),
            selector: version.to_owned(),
            exact: true,
        }
    }

    fn requirement(manager: &str, name: &str, requirement: &str) -> Self {
        Self {
            manager: manager.to_owned(),
            name: name.to_owned(),
            selector: requirement.to_owned(),
            exact: false,
        }
    }
}

fn failure_state(error: &ClientError) -> RouteState {
    let (kind, class) = match error {
        ClientError::Disabled => return RouteState::Disabled,
        ClientError::Config(ConfigError::InvalidTokenEnvironment | ConfigError::InvalidToken)
        | ClientError::CredentialConfiguration => (
            FailureKind::Api,
            GlobalFailureClass::CredentialConfiguration,
        ),
        ClientError::Deadline | ClientError::Cancelled => {
            (FailureKind::Api, GlobalFailureClass::Timeout)
        }
        ClientError::Connection => (FailureKind::Api, GlobalFailureClass::Connection),
        ClientError::ResponseBodyTooLarge { .. } => {
            (FailureKind::Response, GlobalFailureClass::ResponseTruncated)
        }
        ClientError::InvalidResponseField {
            field: "publication_format",
        } => (
            FailureKind::Publication,
            GlobalFailureClass::PublicationFormat,
        ),
        ClientError::InvalidResponseField {
            field: "corpus_revision" | "revision",
        } => (FailureKind::Publication, GlobalFailureClass::CorpusRevision),
        ClientError::InvalidResponseField {
            field: "required_search_fields",
        } => (
            FailureKind::Publication,
            GlobalFailureClass::RequiredFieldSet,
        ),
        ClientError::Http { meta, .. } if matches!(meta.status, 401 | 403) => {
            (FailureKind::Api, GlobalFailureClass::Authentication)
        }
        ClientError::Http { meta, .. } if matches!(meta.status, 429 | 503 | 504) => {
            (FailureKind::Api, GlobalFailureClass::RetryExhausted)
        }
        ClientError::Http { .. } => (FailureKind::Api, GlobalFailureClass::NonSuccessResponse),
        ClientError::Config(_) => (FailureKind::Api, GlobalFailureClass::InvalidResponse),
        ClientError::RequestBodyTooLarge { .. }
        | ClientError::InvalidMediaType { .. }
        | ClientError::InvalidResponse { .. }
        | ClientError::Decode { .. }
        | ClientError::InvalidRequest { .. }
        | ClientError::InvalidResponseField { .. } => {
            (FailureKind::Response, GlobalFailureClass::InvalidResponse)
        }
    };
    RouteState::Unavailable { kind, class }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::layer::SubscriberExt;

    use super::{
        FailureKind, FallbackLogCounts, GlobalRoute, RouteState, ServiceState, failure_state,
        log_fallback_manager, page_window, search_request,
    };
    use rift_cloud_client::{ClientError, ResponseMeta};
    use rift_dependency::DependencyContext;
    use rift_protocol::read::GlobalFailureClass;
    use rift_ranking::{ParsedQuery, QueryPhase};

    #[test]
    fn client_failures_map_to_bounded_warning_classes() {
        assert_eq!(
            failure_state(&ClientError::Connection),
            super::RouteState::Unavailable {
                kind: FailureKind::Api,
                class: GlobalFailureClass::Connection,
            }
        );
        assert_eq!(
            failure_state(&ClientError::InvalidResponseField {
                field: "publication_format",
            }),
            super::RouteState::Unavailable {
                kind: FailureKind::Publication,
                class: GlobalFailureClass::PublicationFormat,
            }
        );
        assert_eq!(
            failure_state(&ClientError::Http {
                meta: Box::new(ResponseMeta {
                    status: 401,
                    content_type: None,
                    etag: None,
                    cache_control: None,
                    www_authenticate: None,
                    retry_after: None,
                    rate_limit_limit: None,
                    rate_limit_remaining: None,
                    rate_limit_reset: None,
                }),
                problem: None,
            }),
            super::RouteState::Unavailable {
                kind: FailureKind::Api,
                class: GlobalFailureClass::Authentication,
            }
        );
    }

    #[test]
    fn search_request_preserves_query_members_and_phase() {
        let params: rift_protocol::read::SearchParams = serde_json::from_value(serde_json::json!({
            "query": "load config",
            "include": ["source"]
        }))
        .expect("search fixture is valid");
        let query = ParsedQuery::parse("load config").expect("query fixture is valid");
        let precise = search_request(&params, &query, &[], QueryPhase::Precise);
        assert_eq!(precise.query, "load config");
        assert_eq!(precise.terms[0].text, "load");
        assert!(!precise.terms[0].prefix);
        let broad = search_request(&params, &query, &[], QueryPhase::Broad);
        assert!(broad.terms.iter().all(|term| term.prefix));
        assert_eq!(broad.include, Some(vec!["source".to_owned()]));
    }

    #[test]
    fn documentation_requests_preserve_targets_and_symbol_includes() {
        use rift_cloud_client::{PackageSearchRequestTarget, PackageSymbolRequestInclude};

        let query = ParsedQuery::parse("compass").expect("query");
        for (target, expected) in [
            ("symbol", None),
            ("file", None),
            (
                "documentation",
                Some(PackageSearchRequestTarget::Documentation),
            ),
            ("all", Some(PackageSearchRequestTarget::All)),
        ] {
            let params = serde_json::from_value(serde_json::json!({
                "query": "compass", "target": target
            }))
            .expect("search request");
            assert_eq!(
                search_request(&params, &query, &[], QueryPhase::Precise).target,
                expected
            );
        }
        let params = serde_json::from_value(serde_json::json!({
            "name": "Compass", "include": ["documentation", "source"]
        }))
        .expect("symbol request");
        assert_eq!(
            super::symbol_include(&params),
            Some(vec![
                PackageSymbolRequestInclude::Source,
                PackageSymbolRequestInclude::Documentation
            ])
        );
    }

    #[tokio::test]
    async fn file_search_returns_no_remote_candidates_with_a_disabled_client() {
        let client = rift_cloud_client::GlobalClient::new(rift_cloud_client::Config {
            enabled: false,
            token_env: String::new(),
            ..rift_cloud_client::Config::default()
        })
        .expect("disabled client");
        let params = serde_json::from_value(serde_json::json!({
            "query": "compass", "target": "file"
        }))
        .expect("file search request");
        let query = ParsedQuery::parse("compass").expect("query");
        let candidates = super::package_search(&client, &params, &query, &[])
            .await
            .expect("file search does not call remote client");
        assert_eq!(candidates, super::GlobalSearchCandidates::default());
    }

    #[test]
    fn documentation_order_uses_block_identity() {
        let directory = tempfile::tempdir().expect("workspace");
        std::fs::write(directory.path().join("guide.txt"), "First.\n\nSecond.\n")
            .expect("documentation source");
        let index = rift_index::WorkspaceIndex::build(
            directory.path(),
            rift_index::WorkspaceIndexLimits::default(),
            &rift_core::SourceVisibility::default(),
            &rift_core::TextFileInclusion::new(vec!["guide.txt".to_owned()], 1024),
        )
        .expect("workspace index");
        let metadata = index.documentation().index();
        let mut hits = metadata
            .blocks
            .iter()
            .map(|block| {
                serde_json::from_value(serde_json::json!({
                    "hit": {"target": "documentation", "documentation": {
                        "documentation_revision": metadata.documentation_revision,
                        "block": block, "source": metadata.sources[0]
                    }},
                    "path": "guide.txt", "range": block.range, "line": block.line
                }))
                .expect("documentation hit")
            })
            .collect::<Vec<_>>();
        assert_eq!(hits.len(), 2);
        let mut expected = metadata
            .blocks
            .iter()
            .map(|block| block.identity.0.as_str())
            .collect::<Vec<_>>();
        expected.sort_unstable();
        for order in [super::ResultOrder::Identity, super::ResultOrder::Path] {
            hits.reverse();
            super::order_search_hits(&mut hits, order);
            assert_eq!(
                hits.iter().map(super::search_hit_key).collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn page_window_reports_empty_page_after_last_page() {
        let (page, pagination) = page_window(vec![1, 2, 3], 4, 2);
        assert!(page.is_empty());
        assert_eq!(pagination.page_index, 4);
        assert_eq!(pagination.total_pages, 2);
    }

    #[test]
    fn service_state_transition_is_logged_once_and_redacted() {
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let observation = Arc::new(Mutex::new(None::<ServiceState>));
        let route = GlobalRoute::fallback(
            &Arc::new(DependencyContext::default()),
            RouteState::Unavailable {
                kind: FailureKind::Api,
                class: GlobalFailureClass::Connection,
            },
            observation,
        );

        tracing::subscriber::with_default(subscriber, || {
            route.record_observation();
            route.record_observation();
        });

        let records = std::iter::from_fn(|| drain.try_recv_record().ok()).collect::<Vec<_>>();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].operation(), "global.state");
        assert!(records[0].fields().contains("unavailable"));
        assert!(records[0].fields().contains("connection"));
        assert!(!records[0].fields().contains("https://"));
        assert!(!records[0].fields().contains("token"));
        assert!(!records[0].fields().contains("private-package"));
    }

    #[test]
    fn fallback_summary_is_bounded_and_uses_manager_only() {
        let (sink, mut drain) = crate::logs::log_capture();
        let subscriber = tracing_subscriber::registry().with(sink);
        let manager = "cargo";

        tracing::subscriber::with_default(subscriber, || {
            log_fallback_manager(
                RouteState::Unavailable {
                    kind: FailureKind::Api,
                    class: GlobalFailureClass::RetryExhausted,
                },
                manager,
                FallbackLogCounts {
                    selected: 3,
                    missing: 1,
                    indexed: 2,
                    unresolved: 1,
                    managers: 1,
                    omitted: 0,
                },
            );
        });

        let record = drain
            .try_recv_record()
            .expect("fallback summary is recorded");
        assert_eq!(record.operation(), "global.fallback");
        assert!(record.fields().contains("retry_exhausted"));
        assert!(record.fields().contains("selected_count"));
        assert!(record.fields().contains("fallback_indexed"));
        assert!(record.fields().contains("fallback_unresolved"));
        assert!(record.fields().contains("partial"));
        assert!(record.fields().contains("fallback_outcome"));
        assert!(record.fields().contains(manager));
        assert!(!record.fields().contains("private-package"));
    }
}
