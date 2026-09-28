//! Global package routing for current-tree reads.

use std::ffi::OsString;
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex};

use percent_encoding::percent_decode_str;
use rift_cloud_client::{
    ClientError, Config, ConfigError, GlobalClient, PackageAvailability as WireAvailability,
    PackageContextEntry as WireContextEntry, PackageIdentity as WirePackageIdentity,
    PackagePatternMatch, PackagePatternRequest, PackageResolutionRequest, PackageSearchCandidate,
    PackageSearchRequest, PackageSearchRequestPhase, PackageSearchRequestTarget,
    PackageSymbolCandidate, PackageSymbolRequest, PackageSymbolRequestInclude, QueryTerm, Warning,
    WarningCode,
};
use rift_dependency::DependencyContext;
use rift_protocol::configuration::GlobalConfiguration;
use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry};
use rift_protocol::read::{
    DEPENDENCY_WARNINGS_MAX, GetSymbolInclude, GetSymbolParams, GetSymbolResult,
    GlobalFailureClass, GlobalPageWarningCode, PackageIdentity, Pagination, ReadWarning,
    ResultOrder, SearchHit, SearchHitTarget, SearchInclude, SearchParams, SearchParamsTarget,
    SearchResult, SearchScope,
};
use rift_ranking::{
    DocumentIdentity, FieldSet, ParsedQuery, QueryPhase, RankedIdentity, RankingInput,
    RankingInputKind, RankingWeights, SearchableField, fuse, match_class,
};
use tokio::sync::Mutex;

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

/// What the global API resolved for one read: the packages it serves, the context
/// entries it holds no release for, the entries a release other than the requested one
/// answers, and the service state the read met.
///
/// A read whose route answers no client carries project hits alone, with the typed
/// global warning naming why.
pub(crate) struct GlobalRoute {
    pub(crate) client: Option<GlobalClient>,
    pub(crate) remote_packages: Vec<WirePackageIdentity>,
    context: Arc<DependencyContext>,
    pub(crate) missing_exact: Vec<PackageIdentity>,
    pub(crate) missing_requirements: Vec<PackageContextEntry>,
    substituted: Vec<SubstitutedEntry>,
    pub(crate) state: RouteState,
    observation: Arc<StdMutex<Option<ServiceState>>>,
}

/// One context entry the global index answers from a collected release other than the one
/// the entry names: an exact version it holds no release of, or a requirement no collected
/// release satisfies.
#[derive(Clone, Debug, PartialEq)]
struct SubstitutedEntry {
    entry: PackageContextEntry,
    package: PackageIdentity,
}

/// The global service state one read met.
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
    /// Resolves the context's canonical entries through the global API.
    pub(crate) async fn route(
        &self,
        configuration: &GlobalConfiguration,
        context: &Arc<DependencyContext>,
    ) -> GlobalRoute {
        let route = self.route_inner(configuration, context).await;
        route.record_observation();
        route
    }

    /// The route of a read whose enclosing MCP request deadline expired before the
    /// global API answered.
    pub(crate) fn deadline_exceeded(&self, context: &Arc<DependencyContext>) -> GlobalRoute {
        let route = GlobalRoute::unanswered(
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
            return GlobalRoute::unanswered(
                context,
                RouteState::Disabled,
                Arc::clone(&self.observation),
            );
        }
        let request = resolution_request(context);
        if request.entries.is_empty() {
            return GlobalRoute::unanswered(
                context,
                RouteState::Available,
                Arc::clone(&self.observation),
            );
        }
        let client = match self.client(configuration).await {
            Ok(client) => client,
            Err(error) => {
                return GlobalRoute::unanswered(
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
            Err(error) => GlobalRoute::unanswered(
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

/// Reads the package matches of one accepted `pattern` search: one page, asked for through
/// the advertised page limit.
///
/// A page that stopped before the last match carries `result_truncated`, which reaches the
/// caller as a `global_page_warning`, and its cursor is not followed.
pub(crate) async fn package_patterns(
    client: &GlobalClient,
    params: &SearchParams,
    packages: &[WirePackageIdentity],
) -> Result<GlobalPatternMatches, ClientError> {
    let Some(pattern) = params.pattern.clone() else {
        return Ok(GlobalPatternMatches::default());
    };
    let request = PackagePatternRequest {
        pattern,
        packages: packages.to_vec(),
        include: includes_source(params).then(|| vec!["source".to_owned()]),
    };
    let page = client
        .search_package_patterns(&request, rift_cloud_client::PAGE_LIMIT_MAX, None)
        .await?;
    let warnings = page_warnings(page.warnings);
    let matches = page
        .items
        .iter()
        .map(PackagePatternMatch::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(GlobalPatternMatches { matches, warnings })
}

/// Package pattern matches and page warnings returned by one remote read.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GlobalPatternMatches {
    pub(crate) matches: Vec<PackagePatternMatch>,
    pub(crate) warnings: Vec<ReadWarning>,
}

fn includes_source(params: &SearchParams) -> bool {
    params
        .include
        .as_deref()
        .unwrap_or_default()
        .contains(&SearchInclude::Source)
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
                WarningCode::RequirementUnsatisfied | WarningCode::Unknown => {
                    GlobalPageWarningCode::Unknown
                }
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
        include: includes_source(params).then(|| vec!["source".to_owned()]),
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
    let limit = search_page_limit(params);
    let (results, pagination) = page_window(ordered, params.page_index, limit);
    Ok(SearchResult {
        results,
        pagination,
        warnings: local.warnings,
    })
}

/// Adds the package matches of a `pattern` search after the project's, then pages the
/// answer.
///
/// Pattern hits carry no score, so `relevance` keeps the collected order: the project's
/// matches by path, then offset, then the packages' in the order the global API answered
/// them, by package, path, and offset. As in the project, a declaration answers once, at
/// its first match, and `target` selects the file hits, the declaration hits, or both.
pub(crate) fn merge_patterns(
    params: &SearchParams,
    local: SearchResult,
    remote: GlobalPatternMatches,
) -> SearchResult {
    let mut hits = local.results;
    let mut declared = std::collections::BTreeSet::new();
    for matched in remote.matches {
        if params.target != SearchParamsTarget::File
            && let Some(declaration) = matched.declaration
            && declared.insert(search_hit_key(&declaration).to_owned())
        {
            hits.push(declaration);
        }
        if params.target != SearchParamsTarget::Symbol {
            hits.push(matched.file);
        }
    }
    order_search_hits(&mut hits, params.order);
    let limit = search_page_limit(params);
    let (results, pagination) = page_window(hits, params.page_index, limit);
    let mut warnings = local.warnings;
    warnings.extend(remote.warnings);
    SearchResult {
        results,
        pagination,
        warnings,
    }
}

/// The page size a search asks for: its `limit`, or the default page size when it names none.
fn search_page_limit(params: &SearchParams) -> usize {
    let limit = params
        .limit
        .unwrap_or(rift_core::constants::SEARCH_RESULTS_DEFAULT as u64);
    usize::try_from(limit).unwrap_or(usize::MAX)
}

/// One page of the project's own symbol hits, for a read whose remote lane failed: the
/// hits keep the order the snapshot ranked them in.
pub(crate) fn local_symbol_page(
    params: &GetSymbolParams,
    local: GetSymbolResult,
) -> GetSymbolResult {
    let limit = usize::try_from(params.limit).unwrap_or(usize::MAX);
    let (hits, pagination) = page_window(local.hits, params.page_index, limit);
    GetSymbolResult {
        hits,
        pagination,
        warnings: local.warnings,
    }
}

/// One page of the project's own search hits, for a read whose remote lane failed: the
/// hits keep the order the snapshot answered them in under `params.order`.
pub(crate) fn local_search_page(params: &SearchParams, local: SearchResult) -> SearchResult {
    let limit = search_page_limit(params);
    let (results, pagination) = page_window(local.results, params.page_index, limit);
    SearchResult {
        results,
        pagination,
        warnings: local.warnings,
    }
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
            rift_protocol::read::MatchedField::Content => Some(SearchableField::FileContent),
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
    /// Discards a remote lane that failed after the resolution: the read answers project
    /// hits alone, with the typed global warning naming the failure.
    pub(crate) fn discard_remote(&mut self, error: &ClientError) {
        self.client = None;
        self.remote_packages.clear();
        self.missing_exact.clear();
        self.missing_requirements.clear();
        self.substituted.clear();
        self.state = failure_state(error);
        self.record_observation();
    }

    /// The warnings a read whose `scope` reaches packages carries: the typed global
    /// warning when the global API did not answer, then at most `DEPENDENCY_WARNINGS_MAX`
    /// package and dependency-context warnings together. Those name each degraded
    /// resolver in resolver order, each context entry no public registry serves with the
    /// missing capability its kind names, each entry the global publication holds no
    /// release for, and each entry a release other than the requested one answers, in that
    /// order.
    pub(crate) fn warnings(&self) -> Vec<ReadWarning> {
        let degraded = self.context.degradations().iter().map(|degradation| {
            ReadWarning::PackageContextDegraded {
                resolver: degradation.resolver.as_str().to_owned(),
                reason: degradation.reason.clone(),
            }
        });
        let unavailable = self.context.unavailable_entries().filter_map(|entry| {
            let reason = entry.availability.unavailable_reason()?;
            Some(ReadWarning::PackageUnavailable {
                entry: entry.clone(),
                reason: reason.to_owned(),
            })
        });
        let absent = self
            .missing_exact
            .iter()
            .cloned()
            .map(|package| ReadWarning::PackageAbsent { package })
            .chain(
                self.missing_requirements
                    .iter()
                    .cloned()
                    .map(|entry| ReadWarning::PackageRequirementAbsent { entry }),
            );
        let substituted =
            self.substituted
                .iter()
                .cloned()
                .map(|substituted| ReadWarning::PackageSubstituted {
                    entry: substituted.entry,
                    package: substituted.package,
                });
        self.state_warning()
            .into_iter()
            .chain(
                degraded
                    .chain(unavailable)
                    .chain(absent)
                    .chain(substituted)
                    .take(DEPENDENCY_WARNINGS_MAX),
            )
            .collect()
    }

    /// The typed warning naming why the global API did not answer, absent when it did.
    fn state_warning(&self) -> Option<ReadWarning> {
        match self.state {
            RouteState::Disabled => Some(ReadWarning::GlobalAccessDisabled),
            RouteState::Available => None,
            RouteState::Unavailable { kind, class } => Some(match kind {
                FailureKind::Api => ReadWarning::GlobalApiUnavailable {
                    failure_class: class,
                },
                FailureKind::Publication => ReadWarning::GlobalPublicationIncompatible {
                    failure_class: class,
                },
                FailureKind::Response => ReadWarning::GlobalResponseInvalid {
                    failure_class: class,
                },
            }),
        }
    }

    /// A route no resolution answered: no client, and no package facts.
    fn unanswered(
        context: &Arc<DependencyContext>,
        state: RouteState,
        observation: Arc<StdMutex<Option<ServiceState>>>,
    ) -> Self {
        Self {
            client: None,
            remote_packages: Vec::new(),
            context: Arc::clone(context),
            missing_exact: Vec::new(),
            missing_requirements: Vec::new(),
            substituted: Vec::new(),
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

/// One entry the resolution named, as a context entry. The client sends canonical entries
/// alone and refuses an answered entry it did not send, so the entry is canonical.
fn protocol_context_entry(entry: WireContextEntry) -> PackageContextEntry {
    PackageContextEntry {
        manager: entry.manager,
        name: entry.name,
        version: entry.version,
        requirement: entry.requirement,
        availability: PackageAvailability::Canonical,
    }
}

fn resolved_route(
    context: &Arc<DependencyContext>,
    client: GlobalClient,
    resolution: rift_cloud_client::PackageResolutionResponse,
    observation: Arc<StdMutex<Option<ServiceState>>>,
) -> GlobalRoute {
    let substituted = resolution
        .substitutions()
        .into_iter()
        .map(|substitution| SubstitutedEntry {
            entry: protocol_context_entry(substitution.requested),
            package: protocol_package_identity(substitution.served),
        })
        .collect();
    let mut remote_packages = Vec::new();
    for package in resolution.available_exact {
        push_distinct_package(&mut remote_packages, package);
    }
    for resolved in resolution.resolved_requirements {
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
        context: Arc::clone(context),
        missing_exact,
        missing_requirements,
        substituted,
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
        FailureKind, GlobalRoute, RouteState, ServiceState, failure_state, page_window,
        search_request,
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

    /// The authored example `T`'s schema states, the value a caller sees documented.
    fn authored_example<T: schemars::JsonSchema + serde::de::DeserializeOwned>() -> T {
        let schema = schemars::schema_for!(T);
        let example = schema
            .get("examples")
            .and_then(|examples| examples.get(0))
            .cloned()
            .expect("the model states an example");
        serde_json::from_value(example).expect("the example is a value of the model")
    }

    /// A read whose remote lane failed pages the project's own hits in the order the
    /// snapshot ranked them, and keeps the snapshot's warnings.
    #[test]
    fn a_local_symbol_page_keeps_the_snapshot_order_and_its_warnings() {
        use rift_protocol::read::{GetSymbolParams, GetSymbolResult, ReadWarning};

        let example: GetSymbolResult = authored_example();
        let hits = ["first", "second", "third"].map(|name| {
            let mut hit = example.hits[0].clone();
            hit.symbol.name = name.to_owned();
            hit
        });
        let local = GetSymbolResult {
            hits: hits.to_vec(),
            pagination: example.pagination,
            warnings: vec![ReadWarning::GlobalAccessDisabled],
        };
        let request = serde_json::json!({"name": "load_config", "limit": 2, "page_index": 1});
        let params: GetSymbolParams = serde_json::from_value(request).expect("a lookup");

        let page = super::local_symbol_page(&params, local);

        let names: Vec<&str> = page
            .hits
            .iter()
            .map(|hit| hit.symbol.name.as_str())
            .collect();
        assert_eq!(names, ["third"]);
        assert_eq!(page.pagination.page_index, 1);
        assert_eq!(page.pagination.total_pages, 2);
        assert_eq!(page.warnings, [ReadWarning::GlobalAccessDisabled]);
    }

    /// The search counterpart pages under the request's `limit`, or the default one when
    /// the request names none.
    #[test]
    fn a_local_search_page_keeps_the_snapshot_order_and_its_warnings() {
        use rift_protocol::read::{ReadWarning, SearchParams, SearchResult};

        let example: SearchResult = authored_example();
        let paths: Vec<_> = example.results.iter().map(|hit| hit.path.clone()).collect();
        let local = |results| SearchResult {
            results,
            pagination: example.pagination.clone(),
            warnings: vec![ReadWarning::GlobalAccessDisabled],
        };

        let request = serde_json::json!({"query": "load_config", "limit": 1, "page_index": 1});
        let params: SearchParams = serde_json::from_value(request).expect("a search");
        let page = super::local_search_page(&params, local(example.results.clone()));
        let paged: Vec<_> = page.results.iter().map(|hit| hit.path.clone()).collect();
        assert_eq!(paged, paths[1..]);
        assert_eq!(page.pagination.total_pages, 2);
        assert_eq!(page.warnings, [ReadWarning::GlobalAccessDisabled]);

        let request = serde_json::json!({"query": "load_config"});
        let params: SearchParams = serde_json::from_value(request).expect("a search");
        let page = super::local_search_page(&params, local(example.results.clone()));
        let paged: Vec<_> = page.results.iter().map(|hit| hit.path.clone()).collect();
        assert_eq!(paged, paths);
        assert_eq!(page.pagination.total_pages, 1);
    }

    /// A context with nothing to resolve sends nothing: the route answers the service as
    /// available and builds no client.
    #[tokio::test]
    async fn a_context_with_nothing_to_resolve_routes_available_without_a_client() {
        let state = super::GlobalState::default();
        let configuration = rift_protocol::configuration::GlobalConfiguration::default();
        let context = Arc::new(DependencyContext::default());

        let route = state.route(&configuration, &context).await;

        assert_eq!(route.state, RouteState::Available);
        assert!(route.client.is_none());
        assert!(route.remote_packages.is_empty());
        assert!(route.warnings().is_empty());
    }

    /// A configuration the client refuses leaves the route unanswered: no client, and the
    /// API failure the read's warning names.
    #[tokio::test]
    async fn a_configuration_the_client_refuses_routes_unavailable_without_a_client() {
        let state = super::GlobalState::default();
        let configuration = rift_protocol::configuration::GlobalConfiguration {
            endpoint: "ftp://global.example.test/rift/rest".to_owned(),
            ..rift_protocol::configuration::GlobalConfiguration::default()
        };
        let context = context_with_path_dependencies(0);

        let route = state.route(&configuration, &context).await;

        let route_state = route.state;
        assert!(
            matches!(
                route_state,
                RouteState::Unavailable {
                    kind: FailureKind::Api,
                    ..
                }
            ),
            "{route_state:?}"
        );
        assert!(route.client.is_none());
        assert!(route.remote_packages.is_empty());
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
        let route = GlobalRoute::unanswered(
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

    /// Inputs holding no file and running no program: every probe fails.
    struct NothingInputs;

    impl rift_dependency::StaticInputs for NothingInputs {
        fn read_file(
            &mut self,
            _path: &std::path::Path,
            _bytes_max: u64,
        ) -> rift_dependency::FileObservation {
            rift_dependency::FileObservation::Absent
        }
    }

    impl rift_dependency::ContextInputs for NothingInputs {
        fn run(
            &mut self,
            command: &rift_dependency::ToolchainCommand,
        ) -> Result<rift_dependency::CommandOutput, rift_dependency::CommandFailure> {
            Err(rift_dependency::CommandFailure {
                program: command.program.to_owned(),
                reason: "not on PATH".to_owned(),
            })
        }
    }

    #[test]
    fn resolution_request_carries_one_standard_library_entry_per_package() {
        use rift_dependency::{StandardLibrary, StandardLibraryRequest, standard_library_answer};
        use rift_protocol::dependencies::ConfiguredPackage;

        // The operator's list stands in for a lockfile pinning `typescript`.
        let configured = [ConfiguredPackage {
            manager: "npm".to_owned(),
            name: "typescript".to_owned(),
            version: Some("5.9.3".to_owned()),
            requirement: None,
        }];
        let mut context = rift_dependency::resolve_context(
            std::path::Path::new("/workspace"),
            &[],
            &[],
            &mut NothingInputs,
            &configured,
        );
        let answer = standard_library_answer(
            &StandardLibraryRequest {
                root: std::path::Path::new("/workspace"),
                libraries: &[StandardLibrary::Rust, StandardLibrary::Node],
                execution: true,
            },
            &mut NothingInputs,
        );
        context.add_standard_libraries(answer);

        let request = super::resolution_request(&context);

        let sent: Vec<(String, String, Option<String>, Option<String>)> = request
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.manager.clone(),
                    entry.name.clone(),
                    entry.version.clone(),
                    entry.requirement.clone(),
                )
            })
            .collect();
        let any = Some(">=0".to_owned());
        assert_eq!(
            sent,
            [
                (
                    "npm".to_owned(),
                    "typescript".to_owned(),
                    Some("5.9.3".to_owned()),
                    None
                ),
                ("stdlib".to_owned(), "node".to_owned(), None, any.clone()),
                ("stdlib".to_owned(), "rust".to_owned(), None, any),
            ]
        );
        let degraded: Vec<&str> = context
            .degradations()
            .iter()
            .map(|degradation| degradation.resolver.as_str())
            .collect();
        assert_eq!(degraded, ["stdlib/rust", "stdlib/node"]);
    }

    /// Inputs holding `files`, each under its absolute path, and running no program.
    struct RecordedFiles(Vec<(std::path::PathBuf, String)>);

    impl rift_dependency::StaticInputs for RecordedFiles {
        fn read_file(
            &mut self,
            path: &std::path::Path,
            _bytes_max: u64,
        ) -> rift_dependency::FileObservation {
            self.0.iter().find(|(held, _)| held == path).map_or(
                rift_dependency::FileObservation::Absent,
                |(_, content)| {
                    rift_dependency::FileObservation::Bytes(content.clone().into_bytes())
                },
            )
        }
    }

    /// A context whose manifest depends on `count` crates by paths outside the workspace,
    /// and whose Rust standard library probe fails.
    fn context_with_path_dependencies(count: usize) -> Arc<DependencyContext> {
        use std::fmt::Write as _;

        use rift_dependency::{StandardLibrary, StandardLibraryRequest, standard_library_answer};

        let root = std::path::Path::new("/workspace");
        let mut manifest =
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\n\n[dependencies]\n".to_owned();
        let mut lockfile =
            "version = 4\n\n[[package]]\nname = \"probe\"\nversion = \"0.1.0\"\n".to_owned();
        for index in 0..count {
            writeln!(
                manifest,
                "outside{index:02} = {{ path = \"../outside{index:02}\" }}"
            )
            .expect("a String takes every write");
            write!(
                lockfile,
                "\n[[package]]\nname = \"outside{index:02}\"\nversion = \"0.1.0\"\n"
            )
            .expect("a String takes every write");
        }
        let mut inputs = RecordedFiles(vec![
            (root.join("Cargo.toml"), manifest),
            (root.join("Cargo.lock"), lockfile),
        ]);
        let visible = [
            rift_protocol::read::ProjectPath("Cargo.lock".to_owned()),
            rift_protocol::read::ProjectPath("Cargo.toml".to_owned()),
        ];
        let mut context = rift_dependency::resolve_context(
            root,
            &visible,
            rift_dependency::resolvers(),
            &mut inputs,
            &[],
        );
        context.add_standard_libraries(standard_library_answer(
            &StandardLibraryRequest {
                root,
                libraries: &[StandardLibrary::Rust],
                execution: true,
            },
            &mut NothingInputs,
        ));
        Arc::new(context)
    }

    fn warning_codes(route: &GlobalRoute) -> Vec<String> {
        route
            .warnings()
            .iter()
            .map(|warning| {
                serde_json::to_value(warning).expect("a warning serializes")["code"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }

    /// The global warning opens the list, then the degraded probe, each entry no public
    /// registry serves, and each package the publication lacks, in that order.
    #[test]
    fn route_warnings_order_the_global_state_then_degraded_unavailable_and_absent() {
        let context = context_with_path_dependencies(2);
        let mut route =
            GlobalRoute::unanswered(&context, RouteState::Disabled, Arc::new(Mutex::new(None)));
        route
            .missing_exact
            .push(rift_protocol::read::PackageIdentity {
                manager: "cargo".to_owned(),
                name: "absent".to_owned(),
                version: "1.0.0".to_owned(),
            });

        assert_eq!(
            warning_codes(&route),
            [
                "global_access_disabled",
                "package_context_degraded",
                "package_unavailable",
                "package_unavailable",
                "package_absent",
            ]
        );
        let warnings = route.warnings();
        let rift_protocol::read::ReadWarning::PackageUnavailable { entry, reason } = &warnings[2]
        else {
            panic!("the third warning names an unserved entry: {warnings:#?}");
        };
        assert_eq!(entry.name, "outside00");
        assert_eq!(
            Some(reason.as_str()),
            rift_protocol::dependencies::PackageAvailability::Path.unavailable_reason()
        );
    }

    /// Each substitution the resolution names warns `package_substituted` after the absent
    /// packages: an exact version answered at another release, and a requirement answered
    /// outside its range. A requirement answered inside its range warns nothing, and a
    /// remote lane discarded after the resolution drops the substitutions with it.
    #[test]
    fn resolved_substitutions_warn_until_the_remote_lane_is_discarded() {
        use rift_protocol::dependencies::{
            PackageAvailability, PackageContextEntry, PackageSelector,
        };
        use rift_protocol::read::{PackageIdentity, ReadWarning};

        let client = rift_cloud_client::GlobalClient::new(rift_cloud_client::Config {
            enabled: false,
            token_env: String::new(),
            ..rift_cloud_client::Config::default()
        })
        .expect("disabled client");
        let resolution: rift_cloud_client::PackageResolutionResponse =
            serde_json::from_value(serde_json::json!({
                "available_exact": [],
                "resolved_requirements": [
                    {
                        "entry": {"manager": "cargo", "name": "demo", "version": "1.0.3",
                                  "availability": "canonical"},
                        "package": {"manager": "cargo", "name": "demo", "version": "1.0.2"}
                    },
                    {
                        "entry": {"manager": "npm", "name": "typescript", "requirement": "~5.7.2",
                                  "availability": "canonical"},
                        "package": {"manager": "npm", "name": "typescript", "version": "5.9.3"}
                    },
                    {
                        "entry": {"manager": "npm", "name": "react", "requirement": "^19",
                                  "availability": "canonical"},
                        "package": {"manager": "npm", "name": "react", "version": "19.1.0"}
                    }
                ],
                "missing_exact": [{"manager": "cargo", "name": "absent", "version": "1.0.0"}],
                "missing_requirements": [],
                "warnings": [{
                    "code": "requirement_unsatisfied",
                    "detail": "npm/typescript ~5.7.2 answered by 5.9.3"
                }]
            }))
            .expect("resolution fixture");
        let mut route = super::resolved_route(
            &Arc::new(DependencyContext::default()),
            client,
            resolution,
            Arc::new(Mutex::new(None)),
        );

        assert_eq!(
            route.remote_packages.len(),
            3,
            "every answering release is read"
        );
        assert_eq!(
            warning_codes(&route),
            [
                "package_absent",
                "package_substituted",
                "package_substituted"
            ]
        );
        let package = |manager: &str, name: &str, version: &str| PackageIdentity {
            manager: manager.to_owned(),
            name: name.to_owned(),
            version: version.to_owned(),
        };
        let substituted = |entry, served| ReadWarning::PackageSubstituted {
            entry,
            package: served,
        };
        assert_eq!(
            route.warnings()[1..],
            [
                substituted(
                    PackageContextEntry::new(
                        "cargo",
                        "demo",
                        PackageSelector::Version("1.0.3".to_owned()),
                        PackageAvailability::Canonical,
                    ),
                    package("cargo", "demo", "1.0.2"),
                ),
                substituted(
                    PackageContextEntry::new(
                        "npm",
                        "typescript",
                        PackageSelector::Requirement("~5.7.2".to_owned()),
                        PackageAvailability::Canonical,
                    ),
                    package("npm", "typescript", "5.9.3"),
                ),
            ]
        );

        route.discard_remote(&ClientError::Connection);
        assert_eq!(warning_codes(&route), ["global_api_unavailable"]);
    }

    /// A package declaration holding two matches answers once, at its first, and `target`
    /// selects the file hits, the declaration hits, or both. The package matches follow the
    /// project's, the page warnings follow the project's warnings, and the merged set pages
    /// under the request's `limit`.
    #[test]
    fn merged_patterns_answer_each_package_declaration_once_after_the_project() {
        use rift_protocol::read::{Pagination, ReadWarning, SearchHit, SearchParams, SearchResult};
        use serde_json::json;

        let unit = "rift://source/cargo/demo@1.0.0/src/lib.rs";
        let package = json!({"manager": "cargo", "name": "demo", "version": "1.0.0"});
        let hit = |value: serde_json::Value| -> SearchHit {
            serde_json::from_value(value).expect("search hit fixture")
        };
        let file = |start: u64| {
            hit(json!({
                "hit": {"target": "file", "size": 45}, "matched_by": ["content"],
                "range": {"start": start, "end": start + 4}, "line": 1, "unit": unit
            }))
        };
        let declaration = hit(json!({
            "hit": {"target": "symbol", "symbol": {
                "id": "rift://symbol/rust/cargo/demo@1.0.0/src/lib.rs/helper_beacon",
                "language": "rust", "name": "helper_beacon", "kind": "function",
                "origin": {"location": "dependency", "package": package, "source_kind": "authored"}
            }},
            "matched_by": ["content"], "range": {"start": 0, "end": 25}, "line": 1, "unit": unit
        }));
        let project = hit(json!({
            "hit": {"target": "file", "size": 10}, "matched_by": ["content"],
            "range": {"start": 0, "end": 4}, "line": 1, "path": "src/lib.rs"
        }));
        let matched = |start: u64| rift_cloud_client::PackagePatternMatch {
            package: rift_protocol::read::PackageIdentity {
                manager: "cargo".to_owned(),
                name: "demo".to_owned(),
                version: "1.0.0".to_owned(),
            },
            file: file(start),
            declaration: Some(declaration.clone()),
        };
        let merged = |request: serde_json::Value| {
            let params: SearchParams = serde_json::from_value(request).expect("search request");
            let local = SearchResult {
                results: vec![project.clone()],
                pagination: Pagination {
                    page_index: 0,
                    total_pages: 1,
                },
                warnings: vec![ReadWarning::GlobalAccessDisabled],
            };
            let remote = super::GlobalPatternMatches {
                matches: vec![matched(4), matched(14)],
                warnings: vec![ReadWarning::GlobalPageWarning {
                    warning_code: rift_protocol::read::GlobalPageWarningCode::ResultTruncated,
                    detail: None,
                }],
            };
            super::merge_patterns(&params, local, remote)
        };

        let all = merged(json!({"pattern": "beacon", "scope": "all", "target": "all"}));
        assert_eq!(
            all.results,
            [project.clone(), declaration.clone(), file(4), file(14)]
        );
        assert_eq!(all.warnings.len(), 2, "{:?}", all.warnings);
        assert_eq!(all.warnings[0], ReadWarning::GlobalAccessDisabled);
        let symbols = merged(json!({"pattern": "beacon", "scope": "all", "target": "symbol"}));
        assert_eq!(symbols.results, [project.clone(), declaration.clone()]);
        let files = merged(json!({"pattern": "beacon", "scope": "all", "target": "file"}));
        assert_eq!(files.results, [project.clone(), file(4), file(14)]);
        let second_page = merged(json!({
            "pattern": "beacon", "scope": "all", "target": "all", "limit": 2, "page_index": 1
        }));
        assert_eq!(second_page.results, [file(4), file(14)]);
        assert_eq!(second_page.pagination.total_pages, 2);
    }

    /// The package warnings stop at `DEPENDENCY_WARNINGS_MAX`; the global warning rides
    /// beside them and is never the one cut.
    #[test]
    fn route_warnings_stop_at_the_dependency_warnings_bound() {
        let bound = rift_protocol::read::DEPENDENCY_WARNINGS_MAX;
        let context = context_with_path_dependencies(bound + 2);
        let answered =
            GlobalRoute::unanswered(&context, RouteState::Available, Arc::new(Mutex::new(None)));
        let refused = GlobalRoute::unanswered(
            &context,
            RouteState::Unavailable {
                kind: FailureKind::Api,
                class: GlobalFailureClass::Connection,
            },
            Arc::new(Mutex::new(None)),
        );

        let answered_codes = warning_codes(&answered);
        assert_eq!(answered_codes.len(), bound, "{answered_codes:?}");
        assert_eq!(answered_codes[0], "package_context_degraded");
        let refused_codes = warning_codes(&refused);
        assert_eq!(refused_codes.len(), bound + 1, "{refused_codes:?}");
        assert_eq!(refused_codes[0], "global_api_unavailable");
        assert_eq!(refused_codes[1..], answered_codes[..]);
    }
}
