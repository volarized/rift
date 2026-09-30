//! Global package routing for current-tree reads.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::sync::{Arc, Mutex as StdMutex};

use percent_encoding::percent_decode_str;
use rift_cloud_client::{
    ClientError, Config, ConfigError, DECLARATION_POSITIONS_MAX, GlobalClient, PACKAGES_MAX,
    POSITION_COMPONENT_MAX, PackageAvailability as WireAvailability,
    PackageContextEntry as WireContextEntry, PackageDeclarationRequest,
    PackageDeclarationRequestPositionEncoding, PackageIdentity as WirePackageIdentity,
    PackagePatternMatch, PackagePatternRequest, PackagePosition, PackageResolutionRequest,
    PackageSearchCandidate, PackageSearchRequest, PackageSearchRequestPhase,
    PackageSearchRequestTarget, PackageSymbolCandidate, PackageSymbolRequest,
    PackageSymbolRequestInclude, QueryTerm, Warning, WarningCode,
};
use rift_core::{ErrorCode, ErrorContext, ErrorName, Fault, fault_label};
use rift_dependency::DependencyContext;
use rift_protocol::configuration::GlobalConfiguration;
use rift_protocol::dependencies::{PackageAvailability, PackageContextEntry, RequestedPackage};
use rift_protocol::read::{
    DEPENDENCY_WARNINGS_MAX, ExactKind, GetSymbolInclude, GetSymbolParams, GetSymbolResult,
    GlobalFailureClass, GlobalPageWarningCode, PackageIdentity, Pagination, ReadWarning,
    ResultOrder, RevisionId, SearchHit, SearchHitTarget, SearchInclude, SearchParams,
    SearchParamsTarget, SearchResult, SearchScope, SymbolId,
};
use rift_ranking::{
    DocumentIdentity, FieldSet, ParsedQuery, QueryPhase, RankedIdentity, RankingInput,
    RankingInputKind, RankingWeights, SearchableField, fuse, match_class,
};
use rift_server::{
    CalleeDeclaration, CalleePackage, PackageCallee, PositionEncoding, ReadError, ReadService,
};
use serde::Serialize;
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

/// The dependency context one current-tree read sends, beside the snapshot context and the
/// request's `packages` it was derived from, so the route can derive it again under a
/// smaller entry bound the global API advertises.
#[derive(Debug)]
pub(crate) struct ReadContext<'a> {
    /// The context under the compiled entry bound, as `ReadService::read_context` answered it.
    context: Arc<DependencyContext>,
    /// The snapshot's own context.
    snapshot: &'a Arc<DependencyContext>,
    /// The request's `packages`.
    requested: &'a [RequestedPackage],
}

impl<'a> ReadContext<'a> {
    /// The context a read of `reads` sends for `requested` under `scope` and `rev`.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] naming `packages` where `ReadService::read_context` refuses
    /// the argument.
    pub(crate) fn accepted(
        reads: &'a ReadService,
        scope: SearchScope,
        rev: Option<&RevisionId>,
        requested: &'a [RequestedPackage],
    ) -> Result<Self, ReadError> {
        Ok(Self {
            context: reads.read_context(scope, rev, requested)?,
            snapshot: reads.dependency_context(),
            requested,
        })
    }

    /// The context a read of `reads` requesting no package sends: the snapshot's own.
    pub(crate) fn snapshot(reads: &'a ReadService) -> Self {
        Self {
            context: Arc::clone(reads.dependency_context()),
            snapshot: reads.dependency_context(),
            requested: &[],
        }
    }

    /// The context this read sends under `entries_max`: its own when it fits, otherwise the
    /// snapshot's with the requested packages applied under that bound.
    fn within(&self, entries_max: usize) -> Arc<DependencyContext> {
        if self.context.entries().len() <= entries_max {
            Arc::clone(&self.context)
        } else {
            Arc::new(self.snapshot.with_requested(self.requested, entries_max))
        }
    }
}

/// What the global API resolved for one read: the packages it serves, the context
/// entries it holds no release for, the entries a release other than the requested one
/// answers, the service state the read met, and the feature its capabilities did not
/// advertise for the read.
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
    unadvertised_feature: Option<&'static str>,
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
    /// Resolves the read's canonical entries through the global API, within the entry
    /// bound its capabilities advertise.
    pub(crate) async fn route(
        &self,
        configuration: &GlobalConfiguration,
        read: &ReadContext<'_>,
    ) -> GlobalRoute {
        let route = self.route_inner(configuration, read).await;
        route.record_observation();
        route
    }

    /// The route of a read whose enclosing MCP request deadline expired before the
    /// global API answered.
    pub(crate) fn deadline_exceeded(&self, read: &ReadContext<'_>) -> GlobalRoute {
        let route = GlobalRoute::unanswered(
            &read.context,
            failure_state(&ClientError::Deadline),
            Arc::clone(&self.observation),
        );
        route.record_observation();
        route
    }

    /// The route of one read. The capabilities come first, so the resolution request
    /// holds the context cut at the entry bound they advertise; a read past it would
    /// otherwise answer no package at all.
    async fn route_inner(
        &self,
        configuration: &GlobalConfiguration,
        read: &ReadContext<'_>,
    ) -> GlobalRoute {
        let context = &read.context;
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
        let capabilities = match client.get_capabilities().await {
            Ok(capabilities) => capabilities,
            Err(error) => {
                return GlobalRoute::unanswered(
                    context,
                    failure_state(&error),
                    Arc::clone(&self.observation),
                );
            }
        };
        let bounded = read.within(capabilities.dependency_entries_max());
        let request = if Arc::ptr_eq(&bounded, context) {
            request
        } else {
            resolution_request(&bounded)
        };
        match client.resolve_package_context(&request).await {
            Ok(resolution) => {
                resolved_route(&bounded, client, resolution, Arc::clone(&self.observation))
            }
            Err(error) => GlobalRoute::unanswered(
                &bounded,
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

/// The declarations the global API named at one walk's package callee positions.
#[derive(Debug, Default)]
pub(crate) struct CalleeDeclarations {
    /// The declaration answered at each position asked, with its kind, by the position's
    /// encoding and key.
    named:
        HashMap<(PackageDeclarationRequestPositionEncoding, CalleePosition), (SymbolId, ExactKind)>,
    /// The releases the resolution served, which name a standard library's version.
    served: Vec<WirePackageIdentity>,
}

/// One package position as a request carries it and its answer names it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CalleePosition {
    package: (String, String, String),
    path: String,
    line: i64,
    character: i64,
}

impl CalleeDeclarations {
    /// The declaration the global API named at `callee`'s position, with its kind and the
    /// exact package holding it; `None` for a position it answered no declaration at, or
    /// one left unasked.
    pub(crate) fn declaration(&self, callee: &PackageCallee) -> Option<CalleeDeclaration> {
        let package = callee_package(callee, &self.served)?;
        let key = (wire_encoding(callee), callee_position(callee, &package)?);
        let (id, kind) = self.named.get(&key)?;
        Some(CalleeDeclaration {
            id: id.clone(),
            kind: kind.clone(),
            package: protocol_package_identity(package),
        })
    }
}

/// Asks the global API which declaration holds each callee's position, one request per
/// position encoding the walk's engines counted in.
///
/// A callee in an installed package is asked at the exact version its install folder
/// names, since a position belongs to one version of a file. A standard library callee is
/// asked at the release the resolution `served` for the library's context entry; a callee
/// of a library the resolution served no release of is left unasked. Past
/// [`DECLARATION_POSITIONS_MAX`] distinct positions in one encoding the rest are left
/// unasked too.
///
/// # Errors
///
/// Returns [`ClientError`] when a request fails; nothing the earlier requests named is
/// kept.
pub(crate) async fn callee_declarations(
    client: &GlobalClient,
    callees: &[PackageCallee],
    served: &[WirePackageIdentity],
) -> Result<CalleeDeclarations, ClientError> {
    let mut named = HashMap::new();
    for request in declaration_requests(callees, served) {
        let answer = client.find_package_declarations(&request).await?;
        let encoding = request.position_encoding;
        for result in answer.results {
            let (Some(declaration), Some(kind)) = (result.declaration, result.kind) else {
                continue;
            };
            let position = result.position;
            let key = CalleePosition {
                package: (
                    position.package.manager,
                    position.package.name,
                    position.package.version,
                ),
                path: position.path,
                line: position.line,
                character: position.character,
            };
            named.insert(
                (encoding.clone(), key),
                (SymbolId(declaration), ExactKind(kind)),
            );
        }
    }
    Ok(CalleeDeclarations {
        named,
        served: served.to_vec(),
    })
}

/// The requests asking for `callees`' positions: one per position encoding, each within
/// the positions bound, every position once.
fn declaration_requests(
    callees: &[PackageCallee],
    served: &[WirePackageIdentity],
) -> Vec<PackageDeclarationRequest> {
    batched_positions(callees.iter().filter_map(|callee| {
        let package = callee_package(callee, served)?;
        Some((wire_encoding(callee), callee_position(callee, &package)?))
    }))
}

/// How the engine that named `callee` counts characters, as a request spells it.
const fn wire_encoding(callee: &PackageCallee) -> PackageDeclarationRequestPositionEncoding {
    match callee.encoding() {
        PositionEncoding::Utf8 => PackageDeclarationRequestPositionEncoding::Utf8,
        PositionEncoding::Utf16 => PackageDeclarationRequestPositionEncoding::Utf16,
    }
}

// A request holds at most `DECLARATION_POSITIONS_MAX` positions, each in one package, so
// it never names more packages than the client's own packages bound.
const _: () = assert!(DECLARATION_POSITIONS_MAX <= PACKAGES_MAX);

/// `positions` batched into one request per encoding, each position once, in the order
/// met. A request stops taking positions at [`DECLARATION_POSITIONS_MAX`]; the positions
/// past it stay unasked.
fn batched_positions(
    positions: impl IntoIterator<Item = (PackageDeclarationRequestPositionEncoding, CalleePosition)>,
) -> Vec<PackageDeclarationRequest> {
    let mut requests: Vec<PackageDeclarationRequest> = Vec::new();
    let mut asked = HashSet::new();
    for (position_encoding, key) in positions {
        let held = requests
            .iter()
            .position(|request| request.position_encoding == position_encoding);
        let index = held.unwrap_or_else(|| {
            requests.push(PackageDeclarationRequest {
                position_encoding: position_encoding.clone(),
                positions: Vec::new(),
            });
            requests.len() - 1
        });
        let request = &mut requests[index];
        if request.positions.len() >= DECLARATION_POSITIONS_MAX
            || !asked.insert((position_encoding, key.clone()))
        {
            continue;
        }
        let (manager, name, version) = key.package;
        request.positions.push(PackagePosition {
            package: WirePackageIdentity {
                manager,
                name,
                version,
            },
            path: key.path,
            line: key.line,
            character: key.character,
        });
    }
    requests
}

/// The exact package a callee's position is asked in: its installed version, or the
/// release the resolution served for its standard library.
fn callee_package(
    callee: &PackageCallee,
    served: &[WirePackageIdentity],
) -> Option<WirePackageIdentity> {
    match callee.package() {
        CalleePackage::Installed(package) => Some(WirePackageIdentity {
            manager: package.manager.clone(),
            name: package.name.clone(),
            version: package.version.clone(),
        }),
        CalleePackage::StandardLibrary(_) => served
            .iter()
            .find(|release| {
                let package = callee.package();
                release.manager == package.manager() && release.name == package.name()
            })
            .cloned(),
    }
}

/// The key of `callee`'s position in `package`; `None` for a line or character past the
/// bound a request carries.
fn callee_position(
    callee: &PackageCallee,
    package: &WirePackageIdentity,
) -> Option<CalleePosition> {
    let position = callee.position();
    let line = i64::from(position.line);
    let character = i64::from(position.character);
    let within_bound = line <= POSITION_COMPONENT_MAX && character <= POSITION_COMPONENT_MAX;
    within_bound.then(|| CalleePosition {
        package: (
            package.manager.clone(),
            package.name.clone(),
            package.version.clone(),
        ),
        path: callee.path().as_str().to_owned(),
        line,
        character,
    })
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
        // A commit search is refused past the `local` scope before any global request, so
        // it never reaches this mapping.
        target: match params.target {
            rift_protocol::read::SearchParamsTarget::Symbol
            | rift_protocol::read::SearchParamsTarget::File
            | rift_protocol::read::SearchParamsTarget::Commit => None,
            rift_protocol::read::SearchParamsTarget::Documentation => {
                Some(PackageSearchRequestTarget::Documentation)
            }
            rift_protocol::read::SearchParamsTarget::All => Some(PackageSearchRequestTarget::All),
        },
    }
}

/// The shares a merge fuses the project's own order and the global index's candidates
/// under: equal parts for the two, no vector share, and the rank constant the
/// reciprocal-rank fusion paper uses.
const MERGE_WEIGHTS: RankingWeights = RankingWeights::fixed(0.5, 0.5, 0.0, 60);

/// Why a merge with package hits could not place one project hit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectHitViolation {
    /// The hit carries no identity: a symbol without `id`, or a file without `path`.
    IdentityMissing,
    /// The symbol address's final segment is not a percent-encoded UTF-8 name.
    IdentityUndecodable,
    /// The requested name matches neither the symbol's name nor its qualified name.
    NameUnmatched,
    /// The hit's `unit` is not a source unit address.
    UnitInvalid,
    /// The ranking refuses the hit's identity: empty, or past its byte bound.
    IdentityRefused,
}

impl ProjectHitViolation {
    /// The refusal naming `hit`, the project hit this violation was found on.
    fn at(self, hit: &str) -> ProjectHitError {
        rift_core::Error::new(ProjectHitFault {
            hit: hit.to_owned(),
            violation: self,
        })
    }
}

impl From<rift_ranking::RankingError> for ProjectHitViolation {
    fn from(_: rift_ranking::RankingError) -> Self {
        Self::IdentityRefused
    }
}

/// A project hit a merge with package hits could not place, named by its wire identity or,
/// lacking one, by its name, or by its kind when it carries no name either.
///
/// The project read builds every hit it answers with the identity the merge keys it by
/// and a name the request matched, so this fault is a broken invariant in that read and
/// classifies as the server's own internal error.
#[derive(Debug)]
pub(crate) struct ProjectHitFault {
    hit: String,
    violation: ProjectHitViolation,
}

impl Fault for ProjectHitFault {
    fn name(&self) -> ErrorName {
        ErrorName::Wire(ErrorCode::InternalError)
    }

    fn context(&self) -> Vec<ErrorContext> {
        vec![
            ErrorContext::new("hit", self.hit.clone()),
            ErrorContext::new("violation", fault_label(&self.violation)),
        ]
    }
}

/// A merge's refusal of one project hit.
pub(crate) type ProjectHitError = rift_core::Error<ProjectHitFault>;

/// Merges local and remote symbol hits, then pages them at `limit`, the request's accepted
/// page size.
///
/// # Errors
///
/// Returns [`ProjectHitError`] naming a project hit that carries no identity, whose
/// identity does not decode to a qualified name, or that the requested name does not
/// match. The project read answers none of these, so each is a broken invariant in that
/// read.
pub(crate) fn merge_symbols(
    params: &GetSymbolParams,
    limit: usize,
    mut local: GetSymbolResult,
    remote: Vec<PackageSymbolCandidate>,
) -> Result<GetSymbolResult, ProjectHitError> {
    let mut entries = Vec::with_capacity(local.hits.len() + remote.len());
    for hit in local.hits.drain(..) {
        let symbol_id = hit
            .symbol
            .id
            .clone()
            .ok_or_else(|| ProjectHitViolation::IdentityMissing.at(&hit.symbol.name))?;
        let package = hit.symbol.origin.package.clone();
        let class = local_match_class(&params.name, &hit, &symbol_id)?;
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
    let (hits, pagination) = page_window(hits, params.page_index, limit);
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

/// How `query` matches one project hit, read from the hit's name and the qualified name
/// its identity encodes.
fn local_match_class(
    query: &str,
    hit: &rift_protocol::read::GetSymbolHit,
    identity: &SymbolId,
) -> Result<rift_ranking::IdentifierMatchClass, ProjectHitError> {
    let qualified_name =
        encoded_qualified_name(identity).map_err(|violation| violation.at(&identity.0))?;
    match_class(
        &query.to_lowercase(),
        &hit.symbol.name.to_lowercase(),
        &qualified_name.to_lowercase(),
    )
    .ok_or_else(|| ProjectHitViolation::NameUnmatched.at(&identity.0))
}

/// The qualified name a symbol address carries, percent-encoded, in its final segment.
fn encoded_qualified_name(identity: &SymbolId) -> Result<Cow<'_, str>, ProjectHitViolation> {
    let encoded = identity
        .0
        .rsplit_once('/')
        .map_or(identity.0.as_str(), |(_, name)| name);
    percent_decode_str(encoded)
        .decode_utf8()
        .map_err(|_| ProjectHitViolation::IdentityUndecodable)
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

/// Merges package candidates into local search hits while retaining traversal-only hits,
/// then pages them at `limit`, the request's accepted page size.
///
/// # Errors
///
/// Returns [`ProjectHitError`] naming a project hit that carries no identity, names a
/// source unit that does not parse, or whose identity the ranking refuses. The project
/// read answers none of these, so each is a broken invariant in that read.
pub(crate) fn merge_search(
    params: &SearchParams,
    limit: usize,
    mut local: SearchResult,
    remote: GlobalSearchCandidates,
) -> Result<SearchResult, ProjectHitError> {
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
    let keep_max = payloads.len().max(1);
    let mut ranked = fuse(
        &[local_input, remote_input],
        MERGE_WEIGHTS,
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
        let broad_ranked = fuse(&[broad_input], MERGE_WEIGHTS, QueryPhase::Broad, keep_max);
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
    let (results, pagination) = page_window(ordered, params.page_index, limit);
    Ok(SearchResult {
        results,
        pagination,
        warnings: local.warnings,
    })
}

/// Adds the package matches of a `pattern` search after the project's, then pages the
/// answer at `limit`, the request's accepted page size.
///
/// Pattern hits carry no score, so `relevance` keeps the collected order: the project's
/// matches by path, then offset, then the packages' in the order the global API answered
/// them, by package, path, and offset. As in the project, a declaration answers once, at
/// its first match, and `target` selects the file hits, the declaration hits, or both.
pub(crate) fn merge_patterns(
    params: &SearchParams,
    limit: usize,
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
    let (results, pagination) = page_window(hits, params.page_index, limit);
    let mut warnings = local.warnings;
    warnings.extend(remote.warnings);
    SearchResult {
        results,
        pagination,
        warnings,
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

/// The ranking identity one project search hit is fused under, or the refusal naming the
/// hit by its wire identity, or by its kind when it carries none.
fn search_identity(hit: &SearchHit) -> Result<DocumentIdentity, ProjectHitError> {
    ranking_identity(hit).map_err(|violation| match search_hit_key(hit) {
        "" => violation.at("file"),
        key => violation.at(key),
    })
}

/// The identity a project search hit ranks under: its symbol address, or the source unit
/// and qualified name for a hit that names a unit, its path for a file, its address for a
/// node, its block for documentation, and its revision for a commit.
fn ranking_identity(hit: &SearchHit) -> Result<DocumentIdentity, ProjectHitViolation> {
    let identity = match (&hit.hit, hit.unit.as_ref(), hit.path.as_ref()) {
        (SearchHitTarget::Symbol { symbol }, unit, _) => {
            let symbol_id = symbol
                .id
                .as_ref()
                .ok_or(ProjectHitViolation::IdentityMissing)?;
            match unit {
                Some(unit) => unit_identity(unit, symbol_id)?,
                None => DocumentIdentity::new(symbol_id.0.clone())?,
            }
        }
        (SearchHitTarget::File { .. }, _, path) => {
            let path = path.ok_or(ProjectHitViolation::IdentityMissing)?;
            DocumentIdentity::new(path.0.clone())?
        }
        (SearchHitTarget::Node { node }, ..) => DocumentIdentity::new(node.0.clone())?,
        (SearchHitTarget::Documentation { documentation }, ..) => {
            DocumentIdentity::for_documentation_block(&documentation.block.identity.0)?
        }
        (SearchHitTarget::Commit { commit }, ..) => {
            DocumentIdentity::new(commit.revision.0.clone())?
        }
    };
    Ok(identity)
}

/// A unit-addressed symbol hit's identity: its source unit and the qualified name its
/// symbol address encodes.
fn unit_identity(
    unit: &rift_protocol::read::SourceUnitId,
    symbol_id: &SymbolId,
) -> Result<DocumentIdentity, ProjectHitViolation> {
    let unit =
        rift_core::SourceUnitId::parse(&unit.0).map_err(|_| ProjectHitViolation::UnitInvalid)?;
    let qualified_name = encoded_qualified_name(symbol_id)?;
    Ok(DocumentIdentity::for_unit(&unit, &qualified_name)?)
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
        SearchHitTarget::Commit { commit } => commit.revision.0.as_str(),
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
    ///
    /// A lane the global API's capabilities do not advertise is no failure of the
    /// service: the resolution's own warnings stand, and the read warns
    /// `capability_unavailable` naming the feature.
    pub(crate) fn discard_remote(&mut self, error: &ClientError) {
        self.client = None;
        self.remote_packages.clear();
        if let ClientError::FeatureUnavailable { feature } = error {
            self.unadvertised_feature = Some(feature);
            return;
        }
        self.missing_exact.clear();
        self.missing_requirements.clear();
        self.substituted.clear();
        self.state = failure_state(error);
        self.record_observation();
    }

    /// The warnings a read whose `scope` reaches packages carries: the typed global
    /// warning when the global API did not answer, or `capability_unavailable` when its
    /// capabilities do not advertise the feature the read needs, then at most
    /// `DEPENDENCY_WARNINGS_MAX` package and dependency-context warnings together. Those
    /// name each degraded resolver in resolver order, each context entry no public registry
    /// serves with the missing capability its kind names, each entry the global publication
    /// holds no release for, and each entry a release other than the requested one answers,
    /// in that order.
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
            .chain(self.unadvertised_feature.map(unadvertised_feature_warning))
            .chain(
                degraded
                    .chain(unavailable)
                    .chain(absent)
                    .chain(substituted)
                    .take(DEPENDENCY_WARNINGS_MAX),
            )
            .collect()
    }

    /// The warnings a read that asked the global API for no package facts of its own
    /// carries: the typed warning naming why the global API did not answer, or
    /// `capability_unavailable` naming the feature its capabilities do not advertise.
    pub(crate) fn service_warnings(&self) -> Vec<ReadWarning> {
        self.state_warning()
            .into_iter()
            .chain(self.unadvertised_feature.map(unadvertised_feature_warning))
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
            unadvertised_feature: None,
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

/// The warning a read carries when the global API's capabilities do not advertise
/// `feature`, which the read needs for its package facts.
fn unadvertised_feature_warning(feature: &'static str) -> ReadWarning {
    ReadWarning::GlobalPageWarning {
        warning_code: GlobalPageWarningCode::CapabilityUnavailable,
        detail: Some(format!(
            "the global API does not advertise the `{feature}` feature, so no package \
             answers this read"
        )),
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

// A read's context, its requested packages applied, holds at most the dependency
// context's own bound, so every resolution request fits the compiled entry bound the
// client holds it to; the route cuts it again at a smaller bound the capabilities advertise.
const _: () = assert!(rift_dependency::PACKAGES_MAX <= rift_cloud_client::DEPENDENCY_ENTRIES_MAX);

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
        unadvertised_feature: None,
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
        | ClientError::FeatureUnavailable { .. }
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
        DocumentIdentity, FailureKind, GlobalRoute, RouteState, SearchHit, SearchHitTarget,
        ServiceState, failure_state, page_window, search_hit_key, search_request,
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
        assert_eq!(
            failure_state(&ClientError::FeatureUnavailable {
                feature: "patterns"
            }),
            super::RouteState::Unavailable {
                kind: FailureKind::Response,
                class: GlobalFailureClass::InvalidResponse,
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

    /// One documentation search hit per block a two-paragraph `guide.txt` holds, in block
    /// order.
    fn documentation_hits() -> Vec<SearchHit> {
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
        metadata
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
            .collect()
    }

    #[test]
    fn documentation_order_uses_block_identity() {
        let mut hits = documentation_hits();
        assert_eq!(hits.len(), 2);
        let mut expected: Vec<String> = hits
            .iter()
            .map(|hit| super::search_hit_key(hit).to_owned())
            .collect();
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

    /// The context a merge's refusal carries: the hit it names and the rule it broke.
    fn refusal_context(hit: &str, violation: &str) -> [rift_core::ErrorContext; 2] {
        [
            rift_core::ErrorContext::new("hit", hit),
            rift_core::ErrorContext::new("violation", violation),
        ]
    }

    /// `hit` with its symbol address replaced; a hit of another kind comes back unchanged.
    fn with_symbol_id(hit: &SearchHit, identity: Option<&str>) -> SearchHit {
        let mut hit = hit.clone();
        if let SearchHitTarget::Symbol { symbol } = &mut hit.hit {
            symbol.id = identity.map(|identity| rift_protocol::read::SymbolId(identity.to_owned()));
        }
        hit
    }

    /// A project hit the lookup ranked merges beside no package hit into one page of its own.
    #[test]
    fn merge_symbols_pages_the_project_hits_it_can_place() {
        use rift_protocol::read::{GetSymbolParams, GetSymbolResult};

        let example: GetSymbolResult = authored_example();
        let request = serde_json::json!({"name": "load_config"});
        let params: GetSymbolParams = serde_json::from_value(request).expect("a lookup");
        let limit = rift_server::accepted_limit(params.limit).expect("an accepted limit");

        let merged = super::merge_symbols(&params, limit, example.clone(), Vec::new())
            .expect("the project hit carries its identity and matches the name");

        assert_eq!(merged.hits, example.hits);
        assert_eq!(merged.pagination.total_pages, 1);
    }

    /// A project hit the merge cannot key or order refuses the lookup as the server's own
    /// internal error, naming the hit and the rule it broke.
    #[test]
    fn merge_symbols_refuses_a_project_hit_it_cannot_place() {
        use rift_protocol::read::{GetSymbolParams, GetSymbolResult, SymbolId};

        let example: GetSymbolResult = authored_example();
        let placed = "rift://symbol/rust/src/config.rs/load_config";
        let undecodable = "rift://symbol/rust/src/config.rs/%FF";
        let cases = [
            ("load_config", None, "load_config", "identity_missing"),
            (
                "load_config",
                Some(undecodable),
                undecodable,
                "identity_undecodable",
            ),
            ("parse_manifest", Some(placed), placed, "name_unmatched"),
        ];
        for (name, identity, hit, violation) in cases {
            let request = serde_json::json!({"name": name});
            let params: GetSymbolParams = serde_json::from_value(request).expect("a lookup");
            let limit = rift_server::accepted_limit(params.limit).expect("an accepted limit");
            let mut local = example.clone();
            local.hits[0].symbol.id = identity.map(|identity| SymbolId(identity.to_owned()));

            let error = super::merge_symbols(&params, limit, local, Vec::new())
                .expect_err("the merge cannot place the project hit");

            assert_eq!(error.descriptor().code(), "internal_error");
            let context = rift_core::Fault::context(error.fault());
            assert_eq!(context, refusal_context(hit, violation));
        }
    }

    /// Each kind of project hit ranks under the identity the fusion keys it by, and a merge
    /// beside no package candidate answers every one of them.
    #[test]
    fn merge_search_ranks_every_kind_of_project_hit() {
        use rift_protocol::read::{SearchParams, SearchResult, SourceUnitId};

        let example: SearchResult = authored_example();
        let unit = "rift://source/rift.sources.project/src/lib.rs";
        let mut unit_hit = example.results[0].clone();
        unit_hit.path = None;
        unit_hit.unit = Some(SourceUnitId(unit.to_owned()));
        let node_hit: SearchHit = serde_json::from_value(serde_json::json!({
            "hit": {"target": "node", "node": "rift://node/rust/src/lib.rs@0-10#dcbef6dd"},
            "path": "src/lib.rs"
        }))
        .expect("a node hit");
        let documentation_hit = documentation_hits().remove(0);
        let revision = "9c1d4e7a2b8f03d5e6a1c4b7d9e2f0a3b5c8d1e4";
        let commit_hit: SearchHit = serde_json::from_value(serde_json::json!({
            "hit": {"target": "commit", "commit": {
                "revision": revision,
                "message": "Bound comparisons at 512 changed paths\n",
                "message_truncated": false,
                "author": {"name": "Alice", "email": "alice@example.com"},
                "timestamp": "2026-09-22T14:03:11+02:00",
                "paths": ["crates/rift-server/src/change.rs"],
                "paths_truncated": false
            }}
        }))
        .expect("a commit hit");
        let block = search_hit_key(&documentation_hit).to_owned();
        let parsed_unit = rift_core::SourceUnitId::parse(unit).expect("a source unit");
        let unit_identity =
            DocumentIdentity::for_unit(&parsed_unit, "load_config").expect("a unit identity");
        let block_identity =
            DocumentIdentity::for_documentation_block(&block).expect("a block identity");
        let hits = [
            example.results[0].clone(),
            unit_hit,
            example.results[1].clone(),
            node_hit,
            documentation_hit,
            commit_hit,
        ];
        let expected = [
            "rift://symbol/rust/src/config.rs/load_config",
            unit_identity.as_str(),
            "src/lib.rs",
            "rift://node/rust/src/lib.rs@0-10#dcbef6dd",
            block_identity.as_str(),
            revision,
        ];

        for (hit, expected) in hits.iter().zip(expected) {
            let identity = super::search_identity(hit).expect("a project hit ranks");
            assert_eq!(identity.as_str(), expected);
        }
        let mut ordered = hits.to_vec();
        super::order_search_hits(&mut ordered, super::ResultOrder::Identity);
        let keys: Vec<&str> = ordered.iter().map(search_hit_key).collect();
        assert!(keys.is_sorted(), "{keys:?}");
        assert!(
            keys.contains(&revision),
            "a commit hit orders by its revision"
        );
        let request = serde_json::json!({"query": "load_config", "limit": 10});
        let params: SearchParams = serde_json::from_value(request).expect("a search");
        let limit = rift_server::search_page_limit(&params).expect("an accepted limit");
        let local = SearchResult {
            results: hits.to_vec(),
            pagination: example.pagination.clone(),
            warnings: Vec::new(),
        };
        let merged = super::merge_search(
            &params,
            limit,
            local,
            super::GlobalSearchCandidates::default(),
        )
        .expect("every project hit ranks");
        assert_eq!(merged.results.len(), hits.len());
    }

    /// A project search hit the merge cannot rank refuses the search as the server's own
    /// internal error, naming the hit, or its kind when it carries no identity.
    #[test]
    fn merge_search_refuses_a_project_hit_it_cannot_place() {
        use rift_protocol::read::{SearchParams, SearchResult, SourceUnitId};

        let example: SearchResult = authored_example();
        let symbol = &example.results[0];
        let undecodable = "rift://symbol/rust/src/config.rs/%FF";
        let oversized = format!(
            "rift://symbol/rust/src/config.rs/{}",
            "a".repeat(rift_ranking::IDENTITY_BYTES_MAX)
        );
        let placed = "rift://symbol/rust/src/config.rs/load_config";
        let with_unit = |hit: SearchHit, unit: &str| SearchHit {
            unit: Some(SourceUnitId(unit.to_owned())),
            ..hit
        };
        let mut pathless = example.results[1].clone();
        pathless.path = None;
        let cases = [
            (
                with_symbol_id(symbol, None),
                "load_config",
                "identity_missing",
            ),
            (pathless, "file", "identity_missing"),
            (
                with_unit(symbol.clone(), "not-a-rift-source-uri"),
                placed,
                "unit_invalid",
            ),
            (
                with_unit(
                    with_symbol_id(symbol, Some(undecodable)),
                    "rift://source/rift.sources.project/src/lib.rs",
                ),
                undecodable,
                "identity_undecodable",
            ),
            (
                with_symbol_id(symbol, Some(oversized.as_str())),
                oversized.as_str(),
                "identity_refused",
            ),
        ];
        let request = serde_json::json!({"query": "load_config"});
        let params: SearchParams = serde_json::from_value(request).expect("a search");
        let limit = rift_server::search_page_limit(&params).expect("an accepted limit");
        for (hit, label, violation) in cases {
            let local = SearchResult {
                results: vec![hit],
                pagination: example.pagination.clone(),
                warnings: Vec::new(),
            };

            let error = super::merge_search(
                &params,
                limit,
                local,
                super::GlobalSearchCandidates::default(),
            )
            .expect_err("the merge cannot place the project hit");

            assert_eq!(error.descriptor().code(), "internal_error");
            let context = rift_core::Fault::context(error.fault());
            assert_eq!(context, refusal_context(label, violation));
        }
    }

    /// A read that sends `context` as it stands, requesting no packages.
    fn read_context(context: &Arc<DependencyContext>) -> super::ReadContext<'_> {
        super::ReadContext {
            context: Arc::clone(context),
            snapshot: context,
            requested: &[],
        }
    }

    /// A context with nothing to resolve sends nothing: the route answers the service as
    /// available and builds no client.
    #[tokio::test]
    async fn a_context_with_nothing_to_resolve_routes_available_without_a_client() {
        let state = super::GlobalState::default();
        let configuration = rift_protocol::configuration::GlobalConfiguration::default();
        let context = Arc::new(DependencyContext::default());

        let route = state.route(&configuration, &read_context(&context)).await;

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

        let route = state.route(&configuration, &read_context(&context)).await;

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

    /// A read that asked the global API for no package facts of its own carries the service
    /// state alone: none of the context's own warnings.
    #[test]
    fn service_warnings_name_the_service_state_alone() {
        let context = context_with_path_dependencies(2);
        let disabled =
            GlobalRoute::unanswered(&context, RouteState::Disabled, Arc::new(Mutex::new(None)));
        assert_eq!(
            disabled.service_warnings(),
            [rift_protocol::read::ReadWarning::GlobalAccessDisabled]
        );
        let available =
            GlobalRoute::unanswered(&context, RouteState::Available, Arc::new(Mutex::new(None)));
        assert!(available.service_warnings().is_empty());
        let mut unadvertised = available;
        unadvertised.discard_remote(&ClientError::FeatureUnavailable {
            feature: "declarations",
        });
        let codes: Vec<String> = unadvertised
            .service_warnings()
            .iter()
            .map(|warning| {
                serde_json::to_value(warning).expect("warning serializes")["code"].to_string()
            })
            .collect();
        assert_eq!(codes, ["\"global_page_warning\""]);
    }

    fn position(name: &str, line: i64) -> super::CalleePosition {
        super::CalleePosition {
            package: ("pypi".to_owned(), name.to_owned(), "1.0.0".to_owned()),
            path: format!("{name}/core.py"),
            line,
            character: 4,
        }
    }

    /// Positions batch into one request per encoding, each position once in the order met.
    #[test]
    fn positions_batch_once_per_encoding() {
        use rift_cloud_client::PackageDeclarationRequestPositionEncoding as Encoding;

        let requests = super::batched_positions([
            (Encoding::Utf16, position("greeting", 0)),
            (Encoding::Utf8, position("greeting", 0)),
            (Encoding::Utf16, position("greeting", 0)),
            (Encoding::Utf16, position("other", 3)),
        ]);
        let batched: Vec<(Encoding, Vec<(String, i64)>)> = requests
            .into_iter()
            .map(|request| {
                let positions = request
                    .positions
                    .into_iter()
                    .map(|position| (position.package.name, position.line))
                    .collect();
                (request.position_encoding, positions)
            })
            .collect();
        assert_eq!(
            batched,
            [
                (
                    Encoding::Utf16,
                    vec![("greeting".to_owned(), 0), ("other".to_owned(), 3)]
                ),
                (Encoding::Utf8, vec![("greeting".to_owned(), 0)]),
            ]
        );
        assert!(super::batched_positions([]).is_empty());
    }

    /// A request stops taking positions at the positions bound.
    #[test]
    fn positions_past_the_bound_stay_unasked() {
        use rift_cloud_client::PackageDeclarationRequestPositionEncoding as Encoding;

        let lines = 0..=i64::try_from(rift_cloud_client::DECLARATION_POSITIONS_MAX)
            .expect("the bound fits a line");
        let requests = super::batched_positions(
            lines.map(|line| (Encoding::Utf8, position("greeting", line))),
        );
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].positions.len(),
            rift_cloud_client::DECLARATION_POSITIONS_MAX
        );
        assert_eq!(
            requests[0].positions.last().map(|position| position.line),
            i64::try_from(rift_cloud_client::DECLARATION_POSITIONS_MAX - 1).ok()
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

    /// A remote lane the capabilities do not advertise drops the lane alone: the read warns
    /// `capability_unavailable` naming the feature, the resolution's own warnings stand, and
    /// the route reads no package.
    #[test]
    fn an_unadvertised_feature_keeps_the_resolution_warnings() {
        use rift_protocol::read::ReadWarning;

        let client = rift_cloud_client::GlobalClient::new(rift_cloud_client::Config {
            enabled: false,
            token_env: String::new(),
            ..rift_cloud_client::Config::default()
        })
        .expect("disabled client");
        let resolution: rift_cloud_client::PackageResolutionResponse =
            serde_json::from_value(serde_json::json!({
                "available_exact": [{"manager": "cargo", "name": "demo", "version": "1.0.0"}],
                "resolved_requirements": [],
                "missing_exact": [{"manager": "cargo", "name": "absent", "version": "1.0.0"}],
                "missing_requirements": []
            }))
            .expect("resolution fixture");
        let mut route = super::resolved_route(
            &Arc::new(DependencyContext::default()),
            client,
            resolution,
            Arc::new(Mutex::new(None)),
        );

        route.discard_remote(&ClientError::FeatureUnavailable {
            feature: "patterns",
        });

        assert!(route.client.is_none());
        assert!(route.remote_packages.is_empty());
        assert_eq!(route.state, RouteState::Available);
        assert_eq!(
            warning_codes(&route),
            ["global_page_warning", "package_absent"]
        );
        assert_eq!(
            route.warnings()[0],
            ReadWarning::GlobalPageWarning {
                warning_code: rift_protocol::read::GlobalPageWarningCode::CapabilityUnavailable,
                detail: Some(
                    "the global API does not advertise the `patterns` feature, so no package \
                     answers this read"
                        .to_owned()
                ),
            }
        );
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
            let limit = rift_server::search_page_limit(&params).expect("an accepted limit");
            super::merge_patterns(&params, limit, local, remote)
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
