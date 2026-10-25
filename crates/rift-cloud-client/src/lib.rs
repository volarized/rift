//! Bounded client for Rift global package data.

mod cache;
pub mod contract;
mod failure;
mod pattern;
pub mod relationships;
mod response;
pub mod search;
mod source;
mod symbol;
/// Opt-out controls for existing requests.
pub mod telemetry;

#[cfg(test)]
#[path = "telemetry/request_tests.rs"]
mod telemetry_requests_tests;

use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use percent_encoding::percent_decode_str;
use reqwest::{
    Client as HttpClient, StatusCode, Url,
    header::{
        ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, ETAG, HeaderMap, HeaderValue,
        IF_NONE_MATCH, RETRY_AFTER, WWW_AUTHENTICATE,
    },
};
use rift_core::FileDigest;
use rift_error::RiftError;
use serde::Serialize;
use tokio::{
    sync::{Mutex, RwLock, Semaphore},
    time::{Instant, sleep, timeout_at},
};

#[expect(
    missing_docs,
    reason = "generated contract source preserves generator output"
)]
#[allow(
    clippy::all,
    clippy::match_same_arms,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::wildcard_imports,
    unreachable_pub,
    unused_imports,
    reason = "generated wire types stay private behind the bounded runtime"
)]
#[rustfmt::skip]
mod generated;
pub use generated::{
    Capabilities, CapabilityBounds, Documentation, DocumentationBlock, DocumentationBlockKind,
    DocumentationContentIdentity, DocumentationContext, DocumentationFormat, DocumentationHit,
    DocumentationLicense, DocumentationReferenceEvidence, DocumentationSelectionReason,
    DocumentationSource, DocumentationSourceFormat, DocumentationSourceIdentity,
    DocumentationStage, DocumentationWarningKind, ExactKind, Extensions, GetCapabilitiesRequest,
    GetCapabilitiesResponse, IdentifierMatchClass, Language, NodeId, NotebookCellIdentity,
    NotebookCellKind, PackageAvailability, PackageContextEntry, PackageDocumentationHit,
    PackageDocumentationHitContributingField, PackageIdentity, PackagePatternDeclaration,
    PackagePatternHit, PackagePatternPage, PackagePatternRequest, PackageResolutionRequest,
    PackageResolutionResponse, PackageSearchHit, PackageSearchHitContributingField,
    PackageSearchRequest, PackageSearchRequestPhase, PackageSearchRequestTarget, Parameter,
    PositionEncoding as PackageDeclarationRequestPositionEncoding, ProblemDetails,
    PublicationFormat, QueryTerm, RelationshipFacet, ResolvePackageContextRequest,
    ResolvePackageContextResponse, ResolvedRequirement, Search200Response,
    SearchPackagePatternsRequest, SearchPackagePatternsRequestQuery, SearchPackagePatternsResponse,
    SearchPackagesRequest, SearchPackagesRequestQuery, SearchPackagesResponse, SearchTraversal,
    Signature, SignatureLink, SourceKind, SourceLocationKind, SourceUnitId, Symbol, SymbolFacet,
    SymbolId, SymbolOrigin, TextRange, TraversalDirection, TypeBinding, TypeBindingOrigin,
    TypeBindingRole, TypeExpression, Warning, WarningCode,
};
pub mod domain;
pub use domain::{PackagePatternMatch, PackageSearchCandidate};
pub use pattern::PATTERN_PAGE_FILES_MAX;
pub use search::{PackageSearchItem, PackageSearchPage};

/// Most positions one declaration request carries.
pub const DECLARATION_POSITIONS_MAX: usize = rift_protocol::source_read::SOURCE_POSITIONS_MAX;

/// Largest line or character one source position carries.
pub const POSITION_COMPONENT_MAX: u64 = rift_protocol::source_read::SOURCE_POSITION_MAX;

/// Default bound for bytes one encoded request body carries.
// The 4 MiB configuration default fits a 32-bit usize.
#[allow(clippy::cast_possible_truncation)]
pub const REQUEST_BODY_BYTES_MAX: usize =
    rift_protocol::configuration::GLOBAL_REQUEST_BYTES_DEFAULT as usize;
/// Default bound for bytes one response body carries.
// The 32 MiB configuration default fits a 32-bit usize.
#[allow(clippy::cast_possible_truncation)]
pub const RESPONSE_BODY_BYTES_MAX: usize =
    rift_protocol::configuration::GLOBAL_RESPONSE_BYTES_DEFAULT as usize;
/// Fewest attempts one request allows.
pub const ATTEMPTS_MIN: u32 = 1;
/// Most attempts one request allows.
pub const ATTEMPTS_MAX: u32 = 5;
/// Most entries one dependency context carries.
pub const DEPENDENCY_ENTRIES_MAX: usize = 20_000;
/// Most characters one package manager name carries, the contract's `maxLength`.
pub const PACKAGE_MANAGER_CHARS_MAX: usize = 128;
/// Most characters one package name carries, the contract's `maxLength`.
pub const PACKAGE_NAME_CHARS_MAX: usize = 4_096;

/// Most characters one canonical registry endpoint carries.
pub const PACKAGE_REGISTRY_CHARS_MAX: usize = 4_096;
/// Most characters one package version, or one version requirement, carries: the contract's
/// `maxLength`.
pub const PACKAGE_VERSION_CHARS_MAX: usize = 4_096;
/// Most characters one capability name, package manager name, or revision label in the
/// capabilities and page metadata carries.
const LABEL_CHARS_MAX: usize = 128;
/// The capability feature a server advertises when search answers documentation blocks.
const DOCUMENTATION_SEARCH_FEATURE: &str = "documentation_search";
/// The capability feature a server advertises when symbol reads attach documentation.
const SYMBOL_DOCUMENTATION_FEATURE: &str = "symbol_documentation";
/// Most characters one query or symbol `name` carries, the unit the contract's `maxLength`
/// counts for both, and the ceiling of the advertised `query_bytes_max`, which counts UTF-8
/// bytes.
pub const QUERY_BYTES_MAX: usize = 4_096;
/// Most terms one query carries.
pub const QUERY_TERMS_MAX: usize = 32;
/// Most characters one query term carries, the unit the contract's `maxLength` counts for
/// `text`, and the ceiling of the advertised `query_term_bytes_max`, which counts UTF-8
/// bytes.
pub const QUERY_TERM_BYTES_MAX: usize = 256;
/// Most identifiers one query carries.
pub const IDENTIFIERS_MAX: usize = 16;
/// Most UTF-8 bytes one query identifier carries.
pub const IDENTIFIER_BYTES_MAX: usize = 4_096;
/// Most packages one read carries.
pub const PACKAGES_MAX: usize = 20_000;
/// Most UTF-8 bytes one cursor carries.
pub const CURSOR_BYTES_MAX: usize = 4_096;
/// Fewest entries one requested page carries.
pub const PAGE_LIMIT_MIN: i64 = 1;
/// Most entries one requested page carries.
///
/// A paged read asks for the smaller of its limit and the advertised `page_limit_max`, so one
/// client pages against a server advertising 200 and reads one page per search phase against a
/// server advertising 1,000.
pub const PAGE_LIMIT_MAX: i64 = 1_000;
/// Most warnings one page assembly retains.
pub const WARNINGS_MAX: usize = 32;
/// Most characters one warning's `detail` carries, on a page or a resolution: room for the
/// longest `requirement_unsatisfied` detail the package fields admit.
pub const WARNING_DETAIL_CHARS_MAX: usize = rift_protocol::read::GLOBAL_WARNING_DETAIL_CHARS_MAX;
/// What a `requirement_unsatisfied` detail writes between the requirement and the version
/// that answers it.
pub(crate) const ANSWERED_BY: &str = " answered by ";
/// Default bound for UTF-8 bytes one source payload carries.
// The 1 MiB configuration default fits a 32-bit usize.
#[allow(clippy::cast_possible_truncation)]
pub const SOURCE_BYTES_MAX: usize =
    rift_protocol::configuration::GLOBAL_SOURCE_BYTES_DEFAULT as usize;
/// Most candidates one page assembly retains.
pub const CANDIDATE_POOL_MAX: usize = 1_000;
/// Most UTF-8 bytes one retained response header carries.
pub const RESPONSE_HEADER_BYTES_MAX: usize = 256;

const DEFAULT_ENDPOINT: &str = "https://api.volar.sh/rift/rest";
const RATE_LIMIT_LIMIT: &str = "ratelimit-limit";
const RATE_LIMIT_REMAINING: &str = "ratelimit-remaining";
const RATE_LIMIT_RESET: &str = "ratelimit-reset";

/// Accepted global client settings.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Config {
    /// Opt-out settings checked before each network request.
    pub telemetry: telemetry::Policy,
    /// Whether global access is enabled.
    pub enabled: bool,
    /// Absolute base URL joined with generated operation paths.
    pub endpoint: String,
    /// Environment variable that can hold a bearer token.
    pub token_env: String,
    /// Connection timeout for each attempt.
    pub connect_timeout: Duration,
    /// Total operation deadline across every attempt and delay.
    pub request_timeout: Duration,
    /// Most attempts one operation makes.
    pub attempts: u32,
    /// Most HTTP requests this client runs at once.
    pub max_in_flight: usize,
    /// Most bytes one encoded request body carries, also bounded by capabilities.
    pub max_request: usize,
    /// Most bytes one response body carries, also bounded by capabilities.
    pub max_response: usize,
    /// Most UTF-8 bytes one source payload carries, also bounded by capabilities.
    pub max_source: usize,
    /// Local ceiling for capability cache lifetime.
    pub capabilities_ttl: Duration,
    /// Local ceiling for package resolution cache lifetime.
    pub resolution_ttl: Duration,
    /// Lifetime for one unavailable state.
    pub failure_ttl: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            telemetry: telemetry::Policy::of_process(std::path::Path::new(".")),
            enabled: true,
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            token_env: "RIFT_API_TOKEN".to_owned(),
            connect_timeout: Duration::from_secs(3),
            request_timeout: Duration::from_secs(15),
            attempts: 3,
            max_in_flight: 4,
            max_request: REQUEST_BODY_BYTES_MAX,
            max_response: RESPONSE_BODY_BYTES_MAX,
            max_source: SOURCE_BYTES_MAX,
            capabilities_ttl: Duration::from_mins(15),
            resolution_ttl: Duration::from_mins(15),
            failure_ttl: Duration::from_secs(30),
        }
    }
}

impl TryFrom<&rift_protocol::configuration::GlobalConfiguration> for Config {
    type Error = RiftError;

    fn try_from(
        value: &rift_protocol::configuration::GlobalConfiguration,
    ) -> Result<Self, Self::Error> {
        let attempts = u32::try_from(value.attempts).map_err(|source| {
            failure::configuration_source("attempts", "outside configured bounds", source)
        })?;
        let max_in_flight = usize::try_from(value.max_in_flight).map_err(|source| {
            failure::configuration_source("max_in_flight", "outside configured bounds", source)
        })?;
        let config = Self {
            telemetry: telemetry::Policy::of_process(std::path::Path::new(".")),
            enabled: value.enabled,
            endpoint: value.endpoint.clone(),
            token_env: value.token_env.clone(),
            connect_timeout: Duration::from_millis(value.connect_timeout.milliseconds()),
            request_timeout: Duration::from_millis(value.request_timeout.milliseconds()),
            attempts,
            max_in_flight,
            max_request: usize::try_from(value.max_request.bytes()).map_err(|source| {
                failure::configuration_source("max_request", "outside configured bounds", source)
            })?,
            max_response: usize::try_from(value.max_response.bytes()).map_err(|source| {
                failure::configuration_source("max_response", "outside configured bounds", source)
            })?,
            max_source: usize::try_from(value.max_source.bytes()).map_err(|source| {
                failure::configuration_source("max_source", "outside configured bounds", source)
            })?,
            capabilities_ttl: Duration::from_millis(value.capabilities_ttl.milliseconds()),
            resolution_ttl: Duration::from_millis(value.resolution_ttl.milliseconds()),
            failure_ttl: Duration::from_millis(value.failure_ttl.milliseconds()),
        };
        if config.enabled {
            validate_config(&config)?;
            validate_endpoint(&config.endpoint)?;
        }
        Ok(config)
    }
}

/// Bounded status and headers retained from one HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseMeta {
    /// HTTP status code.
    pub status: u16,
    /// Response media type when valid UTF-8 and within its bound.
    pub content_type: Option<String>,
    /// Entity tag when present.
    pub etag: Option<String>,
    /// Cache policy when present.
    pub cache_control: Option<String>,
    /// Authentication challenge when present.
    pub www_authenticate: Option<String>,
    /// Retry delay when present.
    pub retry_after: Option<String>,
    /// Rate limit when present.
    pub rate_limit_limit: Option<String>,
    /// Remaining rate limit when present.
    pub rate_limit_remaining: Option<String>,
    /// Rate limit reset time when present.
    pub rate_limit_reset: Option<String>,
}

/// Bounded REST client for global package data.
#[derive(Clone)]
pub struct GlobalClient {
    inner: Arc<Inner>,
    request_admission: Option<Arc<RequestAdmission>>,
}

type RequestAdmission = dyn Fn() -> Result<(), RiftError> + Send + Sync;

/// The name the capabilities flight's waits and holds are recorded under: one
/// capabilities request at a time, held across the request.
const CAPABILITIES_FLIGHT_LOCK: &str = "global.capabilities";
/// The name the resolutions flight's waits and holds are recorded under: one
/// resolution request at a time, held across the request.
const RESOLUTIONS_FLIGHT_LOCK: &str = "global.resolutions";

struct Inner {
    http: HttpClient,
    endpoint: Url,
    enabled: bool,
    config: Config,
    permits: Arc<Semaphore>,
    capabilities: RwLock<Option<CachedCapabilities>>,
    capabilities_flight: Mutex<()>,
    resolution: RwLock<Option<CachedResolution>>,
    resolutions_flight: Mutex<()>,
    failure: RwLock<Option<CachedFailure>>,
    cache_entry_counts: Arc<[AtomicU64; cache::COUNTS]>,
    _cache_entry_reading: Option<rift_tracing::ObservationGuard>,
}

#[derive(Clone)]
struct CachedCapabilities {
    value: Capabilities,
    etag: Option<String>,
    expires: Instant,
}

#[derive(Clone)]
struct CachedResolution {
    analyzer_revision: String,
    corpus_revision: String,
    request_digest: FileDigest,
    value: PackageResolutionResponse,
    expires: Instant,
}

/// One package resolution request with checked JSON bytes retained for reuse.
#[derive(Clone, Debug)]
pub struct PreparedPackageResolutionRequest {
    request: PackageResolutionRequest,
    body: Arc<[u8]>,
    digest: FileDigest,
}

impl PreparedPackageResolutionRequest {
    /// Checks and encodes one package resolution request.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the request breaks a contract bound or the supported JSON
    /// body ceiling. The client applies its configured request bound before sending the bytes.
    pub fn new(request: PackageResolutionRequest) -> Result<Self, RiftError> {
        validate_resolution_request(&request)?;
        Self::from_validated(request)
    }

    fn from_validated(request: PackageResolutionRequest) -> Result<Self, RiftError> {
        let maximum = usize::try_from(rift_protocol::configuration::GLOBAL_REQUEST_BYTES_MAX)
            .map_err(|source| {
                failure::configuration_source("max_request", "outside configured bounds", source)
            })?;
        let body = serialize_body(&request, maximum)?;
        let digest = FileDigest::of(&body);
        Ok(Self {
            request,
            body: Arc::from(body),
            digest,
        })
    }
}

/// The failure that marked the endpoint unavailable, which every request answers until
/// `expires`, so a later read reports the class the first one met.
struct CachedFailure {
    error: RiftError,
    expires: Instant,
}

struct RawResponse {
    status: StatusCode,
    body: Vec<u8>,
    meta: ResponseMeta,
}

/// Search pages assembled under one capability and candidate bound.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageSearchPages {
    /// Ordered, duplicate-free candidates.
    pub items: Vec<PackageSearchItem>,
    /// First distinct warnings in page order.
    pub warnings: Vec<Warning>,
    /// Analyzer revision shared by every page.
    pub analyzer_revision: String,
    /// Corpus revision shared by every page.
    pub corpus_revision: String,
    /// Documentation revision shared by every page that returns documentation.
    pub documentation_revision: Option<String>,
    /// Original captured view and expiry shared by every page.
    pub view: rift_protocol::symbol_read::CapturedView,
    /// Cursor of the last page when the server stopped it at the response body bound. The
    /// assembly does not follow it; a caller **MAY** pass it to `search_packages`.
    pub next_cursor: Option<String>,
}

impl GlobalClient {
    /// Builds one client and captures the configured credential value.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when enabled settings or credentials are invalid.
    pub fn new(config: Config) -> Result<Self, RiftError> {
        let token = if config.enabled {
            read_token(&config.token_env)?
        } else {
            None
        };
        Self::new_with_token(config, token)
    }

    fn new_with_token(config: Config, token: Option<String>) -> Result<Self, RiftError> {
        if config.enabled {
            validate_config(&config)?;
        }
        let endpoint = if config.enabled {
            validate_endpoint(&config.endpoint)?
        } else {
            validate_endpoint(DEFAULT_ENDPOINT)?
        };
        let token = if config.enabled {
            token.map(validate_token).transpose()?
        } else {
            None
        };
        let mut builder = HttpClient::builder()
            .connect_timeout(config.connect_timeout)
            .redirect(reqwest::redirect::Policy::none());
        if let Some(value) = token.as_deref() {
            let mut headers = HeaderMap::new();
            let authorization = format!("Bearer {value}");
            let header = HeaderValue::from_str(&authorization).map_err(|error| {
                failure::configuration_source(
                    "token",
                    "token cannot form a bounded authorization header",
                    error,
                )
            })?;
            headers.insert(AUTHORIZATION, header);
            builder = builder.default_headers(headers);
        }
        let http = builder.build().map_err(|error| {
            failure::configuration_source("http client", "building HTTP client failed", error)
        })?;
        let enabled = config.enabled;
        let max_in_flight = config.max_in_flight;
        let cache_entry_counts = Arc::new(std::array::from_fn(|_| AtomicU64::new(0)));
        let cache_entry_reading = cache::observe(&cache_entry_counts);
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                endpoint,
                enabled,
                permits: Arc::new(Semaphore::new(max_in_flight)),
                capabilities: RwLock::new(None),
                capabilities_flight: Mutex::new(()),
                resolution: RwLock::new(None),
                resolutions_flight: Mutex::new(()),
                failure: RwLock::new(None),
                cache_entry_counts,
                _cache_entry_reading: cache_entry_reading,
                config,
            }),
            request_admission: None,
        })
    }

    /// Applies request-owned admission before each HTTP attempt, including retries.
    /// Cached answers keep their existing owner and do not spend an HTTP request.
    #[must_use]
    pub fn with_request_admission(
        mut self,
        admission: impl Fn() -> Result<(), RiftError> + Send + Sync + 'static,
    ) -> Self {
        self.request_admission = Some(Arc::new(admission));
        self
    }

    /// Configured endpoint, normalized as a URL.
    #[must_use]
    pub fn endpoint(&self) -> &Url {
        &self.inner.endpoint
    }

    /// Reads and caches global API capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when access is disabled or no valid answer is available.
    pub async fn get_capabilities(&self) -> Result<Capabilities, RiftError> {
        rift_tracing::traced!(
            component = "global",
            operation = "get_capabilities",
            async {
                if !self.inner.enabled {
                    return Err(failure::configuration_invalid(
                        "enabled",
                        "global client disabled",
                    ));
                }
                if let Some(failure) = self.recorded_failure().await {
                    return Err(failure);
                }
                let _flight = rift_tracing::lock(CAPABILITIES_FLIGHT_LOCK)
                    .acquire(self.inner.capabilities_flight.lock())
                    .await;
                if let Some(cached) = self.inner.capabilities.read().await.as_ref()
                    && cached.expires > Instant::now()
                {
                    return Ok(cached.value.clone());
                }
                let etag = self
                    .inner
                    .capabilities
                    .read()
                    .await
                    .as_ref()
                    .and_then(|value| value.etag.clone());
                let response = self
                    .request(
                        contract::Endpoint::Capabilities,
                        None,
                        etag.as_deref(),
                        self.inner.config.max_response,
                    )
                    .await;
                let result = match response {
                    Ok(raw) => match response::capabilities(raw).await {
                        Ok(response::ParsedCapabilities::NotModified(meta)) => {
                            let mut cache = self.inner.capabilities.write().await;
                            let Some(cached) = cache.as_mut() else {
                                return Err(failure::response_invalid(
                                    "getCapabilities",
                                    "status",
                                    "unsupported global response status",
                                    Some(304),
                                ));
                            };
                            cached.expires = Instant::now()
                                + bounded_capabilities_ttl(&self.inner.config, &meta);
                            if meta.etag.is_some() {
                                cached.etag = meta.etag;
                            }
                            Ok(cached.value.clone())
                        }
                        Ok(response::ParsedCapabilities::Ok(parsed)) => {
                            let response::Parsed { value, meta } = *parsed;
                            validate_capabilities(&value)?;
                            let ttl = bounded_capabilities_ttl(&self.inner.config, &meta);
                            let cached = CachedCapabilities {
                                value: value.clone(),
                                etag: meta.etag,
                                expires: Instant::now() + ttl,
                            };
                            let mut capabilities = self.inner.capabilities.write().await;
                            *capabilities = Some(cached);
                            self.inner.cache_entry_counts[cache::CAPABILITIES]
                                .store(1, Ordering::Relaxed);
                            drop(capabilities);
                            let mut failure = self.inner.failure.write().await;
                            *failure = None;
                            self.inner.cache_entry_counts[cache::FAILURE]
                                .store(0, Ordering::Relaxed);
                            drop(failure);
                            Ok(value)
                        }
                        Err(error) => Err(error),
                    },
                    Err(error) => Err(error),
                };
                self.observed(result).await
            }
        )
        .await
    }

    /// Resolves one canonical dependency context into exact package identities.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the request or answer breaks the contract, or the endpoint is
    /// unavailable.
    pub async fn resolve_package_context(
        &self,
        request: &PackageResolutionRequest,
    ) -> Result<PackageResolutionResponse, RiftError> {
        rift_tracing::traced!(
            component = "global",
            operation = "resolve_package_context",
            async {
                if !self.inner.enabled {
                    return Err(failure::configuration_invalid(
                        "enabled",
                        "global client disabled",
                    ));
                }
                validate_resolution_request(request)?;
                let capabilities = self.get_capabilities().await?;
                validate_resolution_request_for_capabilities(request, &capabilities)?;
                let prepared = PreparedPackageResolutionRequest::from_validated(request.clone())?;
                validate_body_for_capabilities(
                    &prepared.body,
                    &capabilities,
                    self.inner.config.max_request,
                )?;
                self.resolve_prepared_package_context(&prepared).await
            }
        )
        .await
    }

    /// Resolves one checked package request, reusing its retained bytes and digest.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when capabilities, the answer, or endpoint breaks the contract.
    pub async fn resolve_prepared_package_context(
        &self,
        prepared: &PreparedPackageResolutionRequest,
    ) -> Result<PackageResolutionResponse, RiftError> {
        rift_tracing::traced!(
            component = "global",
            operation = "resolve_prepared_package_context",
            async {
                if !self.inner.enabled {
                    return Err(failure::configuration_invalid(
                        "enabled",
                        "global client disabled",
                    ));
                }
                let capabilities = self.get_capabilities().await?;
                validate_resolution_request_for_capabilities(&prepared.request, &capabilities)?;
                validate_body_for_capabilities(
                    &prepared.body,
                    &capabilities,
                    self.inner.config.max_request,
                )?;
                if let Some(value) = self.inner.resolution.read().await.as_ref()
                    && value.analyzer_revision == capabilities.analyzer_revision
                    && value.corpus_revision == capabilities.corpus_revision
                    && value.request_digest == prepared.digest
                    && value.expires > Instant::now()
                {
                    return Ok(value.value.clone());
                }
                let _flight = rift_tracing::lock(RESOLUTIONS_FLIGHT_LOCK)
                    .acquire(self.inner.resolutions_flight.lock())
                    .await;
                if let Some(value) = self.inner.resolution.read().await.as_ref()
                    && value.analyzer_revision == capabilities.analyzer_revision
                    && value.corpus_revision == capabilities.corpus_revision
                    && value.request_digest == prepared.digest
                    && value.expires > Instant::now()
                {
                    return Ok(value.value.clone());
                }
                let raw = match self
                    .request(
                        contract::Endpoint::Resolutions,
                        Some(prepared.body.to_vec()),
                        None,
                        active_response_body_bytes_max(
                            &capabilities,
                            self.inner.config.max_response,
                        ),
                    )
                    .await
                {
                    Ok(raw) => raw,
                    Err(error) => {
                        return self.observed(Err(error)).await;
                    }
                };
                let response::Parsed { value, meta } = match response::resolution(raw).await {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        return self.observed(Err(error)).await;
                    }
                };
                if let Err(error) = validate_resolution_response(&prepared.request, &value) {
                    return self.observed(Err(error)).await;
                }
                let ttl = bounded_resolution_ttl(&self.inner.config, &meta);
                let mut resolution = self.inner.resolution.write().await;
                *resolution = Some(CachedResolution {
                    analyzer_revision: capabilities.analyzer_revision.clone(),
                    corpus_revision: capabilities.corpus_revision.clone(),
                    request_digest: prepared.digest,
                    value: value.clone(),
                    expires: Instant::now() + ttl,
                });
                self.inner.cache_entry_counts[cache::RESOLUTION].store(1, Ordering::Relaxed);
                drop(resolution);
                Ok(value)
            }
        )
        .await
    }

    /// Reads one package search page.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when the request or answer breaks the contract, or the endpoint is
    /// unavailable.
    pub async fn search_packages(
        &self,
        request: &PackageSearchRequest,
        limit: i64,
        cursor: Option<&str>,
    ) -> Result<PackageSearchPage, RiftError> {
        rift_tracing::traced!(component = "global", operation = "search_packages", async {
            if !self.inner.enabled {
                return Err(failure::configuration_invalid(
                    "enabled",
                    "global client disabled",
                ));
            }
            validate_search_request(request)?;
            validate_page(limit, cursor)?;
            let capabilities = self.get_capabilities().await?;
            validate_search_request_for_capabilities(request, limit, cursor, &capabilities)?;
            let body = serialize_body(request, self.inner.config.max_request)?;
            validate_body_for_capabilities(&body, &capabilities, self.inner.config.max_request)?;
            let query = SearchPackagesRequestQuery {
                limit: Some(limit),
                cursor: cursor.map(str::to_owned),
            };
            let raw = match self
                .request_with_query(
                    contract::Endpoint::Search,
                    body,
                    &query,
                    active_response_body_bytes_max(&capabilities, self.inner.config.max_response),
                )
                .await
            {
                Ok(raw) => raw,
                Err(error) => {
                    return self.observed(Err(error)).await;
                }
            };
            let response::Parsed { value: page, .. } = match response::search(raw) {
                Ok(parsed) => parsed,
                Err(error) => {
                    return self.observed(Err(error)).await;
                }
            };
            if let Err(error) = validate_search_page(
                request,
                &capabilities,
                &page,
                cursor,
                self.inner.config.max_source,
            ) {
                return self.observed(Err(error)).await;
            }
            Ok(page)
        })
        .await
    }

    /// Reads package search pages through the active candidate bound, asking each page for the
    /// smaller of `limit` and the advertised `page_limit_max`.
    ///
    /// A page the server stopped at the response body bound ends the assembly, and its cursor
    /// lands in [`PackageSearchPages::next_cursor`] unfollowed.
    ///
    /// # Errors
    ///
    /// Returns [`RiftError`] when any page is unavailable, changes revision, repeats an identity,
    /// or repeats a cursor.
    pub async fn search_packages_pages(
        &self,
        request: &PackageSearchRequest,
        limit: i64,
    ) -> Result<PackageSearchPages, RiftError> {
        rift_tracing::traced!(
            component = "global",
            operation = "search_packages_pages",
            async {
                validate_search_request(request)?;
                validate_page(limit, None)?;
                let capabilities = self.get_capabilities().await?;
                let limit = advertised_page_limit(limit, &capabilities);
                validate_search_request_for_capabilities(request, limit, None, &capabilities)?;
                let candidate_max = bounded_candidate_pool(&capabilities);
                let mut cursor = None;
                let mut hits = Vec::new();
                let mut warnings = Vec::new();
                let mut revisions = None;
                let mut captured_view = None;
                let mut captured_request = request.clone();
                let mut seen_identities = HashSet::new();
                let mut seen_cursors = HashSet::new();
                loop {
                    let page = self
                        .search_packages(&captured_request, limit, cursor.as_deref())
                        .await?;
                    if let Some(view) = &captured_view {
                        if view != &page.view {
                            return self
                                .observed(Err(failure::response_invalid(
                                    "search_packages_pages",
                                    "view",
                                    "captured view changed between pages",
                                    None,
                                )))
                                .await;
                        }
                    } else {
                        captured_request.view = Some(page.view.id.as_str().to_owned());
                        captured_view = Some(page.view.clone());
                    }
                    if let Err(error) = validate_assembled_revision(
                        &mut revisions,
                        &page.analyzer_revision,
                        &page.corpus_revision,
                        page.documentation_revision.as_deref(),
                    ) {
                        return self.observed(Err(error)).await;
                    }
                    let stopped = stopped_at_body_bound(&page.warnings, page.items.len(), limit);
                    extend_warnings(&mut warnings, page.warnings);
                    for hit in page.items {
                        let identity = search_hit_identity(&hit)?;
                        if !seen_identities.insert(identity) {
                            return self
                                .observed(Err(failure::response_invalid(
                                    "search_packages_pages",
                                    "duplicate_item",
                                    "field failed validation",
                                    None,
                                )))
                                .await;
                        }
                        hits.push(hit);
                        if hits.len() >= candidate_max {
                            return search_pages(hits, warnings, revisions, captured_view, None);
                        }
                    }
                    let Some(next) = page.next_cursor else {
                        return search_pages(hits, warnings, revisions, captured_view, None);
                    };
                    if stopped {
                        return search_pages(hits, warnings, revisions, captured_view, Some(next));
                    }
                    if !seen_cursors.insert(next.clone()) {
                        return self
                            .observed(Err(failure::response_invalid(
                                "search_packages_pages",
                                "cursor_progress",
                                "field failed validation",
                                None,
                            )))
                            .await;
                    }
                    cursor = Some(next);
                }
            }
        )
        .await
    }

    async fn request_with_query<Q: Serialize + Sync + ?Sized>(
        &self,
        operation: contract::Endpoint,
        body: Vec<u8>,
        query: &Q,
        response_body_bytes_max: usize,
    ) -> Result<RawResponse, RiftError> {
        self.request_inner(
            operation,
            Some(body),
            Some(query),
            None,
            response_body_bytes_max,
        )
        .await
    }

    async fn request(
        &self,
        operation: contract::Endpoint,
        body: Option<Vec<u8>>,
        etag: Option<&str>,
        response_body_bytes_max: usize,
    ) -> Result<RawResponse, RiftError> {
        self.request_inner::<()>(operation, body, None, etag, response_body_bytes_max)
            .await
    }

    async fn request_inner<Q: Serialize + Sync + ?Sized>(
        &self,
        operation: contract::Endpoint,
        body: Option<Vec<u8>>,
        query: Option<&Q>,
        etag: Option<&str>,
        response_body_bytes_max: usize,
    ) -> Result<RawResponse, RiftError> {
        let deadline = Instant::now() + self.inner.config.request_timeout;
        let attempts = self.inner.config.attempts;
        let span = rift_tracing::info_span!(
            "global.request",
            component = "global",
            operation = operation.operation_id(),
            request_count = rift_tracing::empty!(),
            response_bytes = rift_tracing::empty!(),
            status = rift_tracing::empty!(),
            retry_count = rift_tracing::empty!(),
        );
        let record = span.clone();
        span.instrument(async move {
            let mut attempt = 0;
            loop {
                if let Some(admission) = &self.request_admission {
                    admission()?;
                }
                attempt += 1;
                let result = timeout_at(
                    deadline,
                    self.send_once(
                        operation,
                        body.as_deref(),
                        query,
                        etag,
                        response_body_bytes_max,
                    ),
                )
                .await;
                let response = match result {
                    Ok(Ok(response)) => response,
                    Ok(Err(error)) => {
                        if failure::retryable_connection(&error)
                            && attempt < attempts
                            && Instant::now() < deadline
                        {
                            continue;
                        }
                        record_request_span(&record, attempt, None);
                        return Err(error);
                    }
                    Err(source) => {
                        record_request_span(&record, attempt, None);
                        return Err(failure::request_source(
                            operation.operation_id(),
                            "global request deadline elapsed",
                            source,
                        ));
                    }
                };
                rift_tracing::debug!(
                    operation = operation.operation_id(),
                    attempt,
                    retry_count = attempt.saturating_sub(1),
                    status = response.status.as_u16(),
                    response_bytes = response.body.len(),
                    "global response"
                );
                let exact_outcome = operation == contract::Endpoint::Symbols
                    && response.status == StatusCode::SERVICE_UNAVAILABLE
                    && is_media(&response.meta, "application/json");
                if !exact_outcome
                    && !terminal_search_outcome(operation, &response)
                    && should_retry(response.status, attempt, attempts)
                {
                    rift_tracing::debug!(
                        operation = operation.operation_id(),
                        attempt,
                        status = response.status.as_u16(),
                        "retrying global request"
                    );
                    if let Some(delay) = retry_delay(&response.meta) {
                        if delay >= deadline.saturating_duration_since(Instant::now()) {
                            record_request_span(&record, attempt, Some(&response));
                            return Ok(response);
                        }
                        if let Err(source) = timeout_at(deadline, sleep(delay)).await {
                            record_request_span(&record, attempt, None);
                            return Err(failure::request_source(
                                operation.operation_id(),
                                "global request deadline elapsed",
                                source,
                            ));
                        }
                    }
                    continue;
                }
                record_request_span(&record, attempt, Some(&response));
                return Ok(response);
            }
        })
        .await
    }

    async fn send_once<Q: Serialize + Sync + ?Sized>(
        &self,
        operation: contract::Endpoint,
        body: Option<&[u8]>,
        query: Option<&Q>,
        etag: Option<&str>,
        response_body_bytes_max: usize,
    ) -> Result<RawResponse, RiftError> {
        let _permit =
            self.inner.permits.acquire().await.map_err(|error| {
                failure::request_cancelled_source(operation.operation_id(), error)
            })?;
        let url = make_url(&self.inner.endpoint, operation.path());
        let method = operation.method();
        let mut request = self
            .inner
            .http
            .request(method, url)
            .header(ACCEPT, "application/json, application/problem+json");
        if self.inner.config.telemetry.disabled().await {
            request = request.header(reqwest::header::DNT, "1");
        }
        if let Some(query) = query {
            request = request.query(query);
        }
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(body) = body {
            request = request
                .header(CONTENT_TYPE, "application/json")
                .body(body.to_vec());
        }
        let mut response = request.send().await.map_err(|error| {
            if error.is_timeout() && error.is_connect() {
                failure::request_source(
                    operation.operation_id(),
                    "global request deadline elapsed",
                    error,
                )
            } else {
                failure::request_source(
                    operation.operation_id(),
                    "global endpoint connection failed",
                    error,
                )
            }
        })?;
        let status = response.status();
        let meta = ResponseMeta::from_headers(status, response.headers())?;
        if status == StatusCode::NOT_MODIFIED {
            return Ok(RawResponse {
                status,
                body: Vec::new(),
                meta,
            });
        }
        if let Some(size) = response.content_length()
            && size > response_body_bytes_max as u64
        {
            let bytes = usize::try_from(size).unwrap_or(usize::MAX);
            return Err(failure::body_limit(
                operation.operation_id(),
                "response",
                bytes,
                response_body_bytes_max,
            ));
        }
        let mut body_bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            failure::request_source(
                operation.operation_id(),
                "global endpoint connection failed",
                error,
            )
        })? {
            let next = body_bytes.len().saturating_add(chunk.len());
            if next > response_body_bytes_max {
                return Err(failure::body_limit(
                    operation.operation_id(),
                    "response",
                    next,
                    response_body_bytes_max,
                ));
            }
            body_bytes.extend_from_slice(&chunk);
        }
        if status.is_success()
            && status != StatusCode::NO_CONTENT
            && !is_media(&meta, "application/json")
        {
            return Err(failure::response_invalid(
                operation.operation_id(),
                "content_type",
                &format!("invalid response media type: {:?}", meta.content_type),
                Some(status.as_u16()),
            ));
        }
        Ok(RawResponse {
            status,
            body: body_bytes,
            meta,
        })
    }

    /// Hands `result` back, first recording its failure as the one every request answers
    /// for `failure_ttl`.
    async fn observed<T>(&self, result: Result<T, RiftError>) -> Result<T, RiftError> {
        if let Err(error) = &result {
            let mut failure = self.inner.failure.write().await;
            *failure = Some(CachedFailure {
                error: error.clone(),
                expires: Instant::now() + self.inner.config.failure_ttl,
            });
            self.inner.cache_entry_counts[cache::FAILURE].store(1, Ordering::Relaxed);
            drop(failure);
        }
        result
    }

    /// The failure the endpoint answers while it is marked unavailable.
    async fn recorded_failure(&self) -> Option<RiftError> {
        self.inner
            .failure
            .read()
            .await
            .as_ref()
            .filter(|failure| failure.expires > Instant::now())
            .map(|failure| failure.error.clone())
    }
}

fn record_request_span(
    span: &rift_tracing::Span,
    request_count: u32,
    response: Option<&RawResponse>,
) {
    span.record("request_count", request_count);
    span.record("retry_count", request_count.saturating_sub(1));
    if let Some(response) = response {
        span.record("status", response.status.as_u16());
        span.record("response_bytes", response.body.len());
    }
}

impl ResponseMeta {
    fn from_headers(status: StatusCode, headers: &HeaderMap) -> Result<Self, RiftError> {
        Ok(Self {
            status: status.as_u16(),
            content_type: header_text(headers, CONTENT_TYPE.as_str(), "content_type")?,
            etag: header_text(headers, ETAG.as_str(), "etag")?,
            cache_control: header_text(headers, CACHE_CONTROL.as_str(), "cache_control")?,
            www_authenticate: header_text(headers, WWW_AUTHENTICATE.as_str(), "www_authenticate")?,
            retry_after: header_text(headers, RETRY_AFTER.as_str(), "retry_after")?,
            rate_limit_limit: header_text(headers, RATE_LIMIT_LIMIT, "rate_limit_limit")?,
            rate_limit_remaining: header_text(
                headers,
                RATE_LIMIT_REMAINING,
                "rate_limit_remaining",
            )?,
            rate_limit_reset: header_text(headers, RATE_LIMIT_RESET, "rate_limit_reset")?,
        })
    }
}

fn capabilities_bounds_valid(value: &Capabilities) -> bool {
    let bounds = &value.bounds;
    usize::try_from(rift_protocol::configuration::GLOBAL_REQUEST_BYTES_MAX)
        .is_ok_and(|maximum| positive_within(bounds.request_body_bytes_max, maximum))
        && usize::try_from(rift_protocol::configuration::GLOBAL_RESPONSE_BYTES_MAX)
            .is_ok_and(|maximum| positive_within(bounds.response_body_bytes_max, maximum))
        && positive_within(bounds.dependency_entries_max, DEPENDENCY_ENTRIES_MAX)
        && positive_within(bounds.query_bytes_max, QUERY_BYTES_MAX)
        && positive_within(bounds.query_terms_max, QUERY_TERMS_MAX)
        && positive_within(bounds.query_term_bytes_max, QUERY_TERM_BYTES_MAX)
        && positive_within(bounds.identifiers_max, IDENTIFIERS_MAX)
        && positive_within(bounds.identifier_bytes_max, IDENTIFIER_BYTES_MAX)
        && positive_within(bounds.packages_max, PACKAGES_MAX)
        && (PAGE_LIMIT_MIN..=PAGE_LIMIT_MAX).contains(&bounds.page_limit_min)
        && (PAGE_LIMIT_MIN..=PAGE_LIMIT_MAX).contains(&bounds.page_limit_max)
        && (PAGE_LIMIT_MIN..=PAGE_LIMIT_MAX).contains(&bounds.page_limit_default)
        && positive_within(bounds.cursor_bytes_max, CURSOR_BYTES_MAX)
        && positive_within(bounds.candidate_pool_max, CANDIDATE_POOL_MAX)
        && positive_within(bounds.warnings_max, WARNINGS_MAX)
        && usize::try_from(rift_protocol::configuration::GLOBAL_SOURCE_BYTES_MAX)
            .is_ok_and(|maximum| positive_within(bounds.source_bytes_max, maximum))
        && bounds.page_limit_min <= bounds.page_limit_max
        && bounds.page_limit_default >= bounds.page_limit_min
        && bounds.page_limit_default <= bounds.page_limit_max
}

fn validate_capabilities(value: &Capabilities) -> Result<(), RiftError> {
    if value.publication_format != PublicationFormat::RiftPackageIndexV2 {
        return Err(failure::response_invalid(
            "validate_capabilities",
            "publication_format",
            "field failed validation",
            None,
        ));
    }
    if value.supported_package_managers.is_empty()
        || value.supported_package_managers.len() > 3
        || value
            .supported_package_managers
            .iter()
            .any(|manager| !within_characters(manager, LABEL_CHARS_MAX))
    {
        return Err(failure::response_invalid(
            "validate_capabilities",
            "supported_package_managers",
            "field failed validation",
            None,
        ));
    }
    let required_features = ["resolutions", "search", "symbols"];
    if value.supported_features.is_empty()
        || value.supported_features.len() > 32
        || value
            .supported_features
            .iter()
            .any(|feature| !within_characters(feature, LABEL_CHARS_MAX))
        || required_features.iter().any(|required| {
            !value
                .supported_features
                .iter()
                .any(|feature| feature == required)
        })
    {
        return Err(failure::response_invalid(
            "validate_capabilities",
            "supported_features",
            "field failed validation",
            None,
        ));
    }
    if !within_characters(&value.analyzer_revision, LABEL_CHARS_MAX)
        || !within_characters(&value.corpus_revision, LABEL_CHARS_MAX)
    {
        return Err(failure::response_invalid(
            "validate_capabilities",
            "revision",
            "field failed validation",
            None,
        ));
    }
    let documentation_supported = supports_feature(value, DOCUMENTATION_SEARCH_FEATURE)
        || supports_feature(value, SYMBOL_DOCUMENTATION_FEATURE);
    if documentation_supported
        != value
            .documentation_revision
            .as_deref()
            .is_some_and(valid_revision_digest)
    {
        return Err(failure::response_invalid(
            "validate_capabilities",
            "documentation_revision",
            "field failed validation",
            None,
        ));
    }
    if !capabilities_bounds_valid(value) {
        return Err(failure::response_invalid(
            "validate_capabilities",
            "bounds",
            "field failed validation",
            None,
        ));
    }
    let known_fields = ["name", "qualified_name", "documentation", "signature"];
    if value.required_search_fields.len() != known_fields.len()
        || known_fields.iter().any(|required| {
            !value
                .required_search_fields
                .iter()
                .any(|field| field == required)
        })
    {
        return Err(failure::response_invalid(
            "validate_capabilities",
            "required_search_fields",
            "field failed validation",
            None,
        ));
    }
    Ok(())
}

fn supports_feature(capabilities: &Capabilities, feature: &str) -> bool {
    capabilities
        .supported_features
        .iter()
        .any(|supported| supported == feature)
}

fn valid_revision_digest(value: &str) -> bool {
    value.len() == 8
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_resolution_request_for_capabilities(
    request: &PackageResolutionRequest,
    capabilities: &Capabilities,
) -> Result<(), RiftError> {
    if request.entries.len() > capabilities.dependency_entries_max() {
        return Err(failure::request_invalid(
            "validate_resolution_request_for_capabilities",
            "entries",
            "field failed validation",
        ));
    }
    Ok(())
}

fn validate_search_request_for_capabilities(
    request: &PackageSearchRequest,
    limit: i64,
    cursor: Option<&str>,
    capabilities: &Capabilities,
) -> Result<(), RiftError> {
    let target = request
        .target
        .as_ref()
        .unwrap_or(&PackageSearchRequestTarget::Symbol);
    if matches!(
        target,
        PackageSearchRequestTarget::Documentation | PackageSearchRequestTarget::All
    ) && !supports_feature(capabilities, DOCUMENTATION_SEARCH_FEATURE)
    {
        return Err(failure::configuration_invalid(
            "feature",
            &format!("global API does not advertise feature: {DOCUMENTATION_SEARCH_FEATURE}"),
        ));
    }
    let bounds = &capabilities.bounds;
    if request
        .query
        .as_ref()
        .is_some_and(|query| query.len() > smaller_bound(bounds.query_bytes_max, QUERY_BYTES_MAX))
    {
        return Err(failure::request_invalid(
            "validate_search_request_for_capabilities",
            "query",
            "field failed validation",
        ));
    }
    let terms = request.terms.as_deref().unwrap_or_default();
    if terms.len() > smaller_bound(bounds.query_terms_max, QUERY_TERMS_MAX)
        || terms.iter().any(|term| {
            term.text.len() > smaller_bound(bounds.query_term_bytes_max, QUERY_TERM_BYTES_MAX)
        })
    {
        return Err(failure::request_invalid(
            "validate_search_request_for_capabilities",
            "terms",
            "field failed validation",
        ));
    }
    let identifiers = request.identifiers.as_deref().unwrap_or_default();
    if identifiers.len() > smaller_bound(bounds.identifiers_max, IDENTIFIERS_MAX)
        || identifiers.iter().any(|identifier| {
            identifier.len() > smaller_bound(bounds.identifier_bytes_max, IDENTIFIER_BYTES_MAX)
        })
    {
        return Err(failure::request_invalid(
            "validate_search_request_for_capabilities",
            "identifiers",
            "field failed validation",
        ));
    }
    validate_read_bounds(
        request.packages.as_ref().map_or(0, Vec::len),
        limit,
        cursor,
        capabilities,
    )
}

fn validate_read_bounds(
    packages: usize,
    limit: i64,
    cursor: Option<&str>,
    capabilities: &Capabilities,
) -> Result<(), RiftError> {
    let bounds = &capabilities.bounds;
    if packages > smaller_bound(bounds.packages_max, PACKAGES_MAX) {
        return Err(failure::request_invalid(
            "validate_read_bounds",
            "packages",
            "field failed validation",
        ));
    }
    if limit < bounds.page_limit_min || limit > bounds.page_limit_max {
        return Err(failure::request_invalid(
            "validate_read_bounds",
            "limit",
            "field failed validation",
        ));
    }
    if cursor
        .is_some_and(|value| value.len() > smaller_bound(bounds.cursor_bytes_max, CURSOR_BYTES_MAX))
    {
        return Err(failure::request_invalid(
            "validate_read_bounds",
            "cursor",
            "field failed validation",
        ));
    }
    Ok(())
}

fn validate_resolution_request(request: &PackageResolutionRequest) -> Result<(), RiftError> {
    if request.entries.len() > DEPENDENCY_ENTRIES_MAX {
        return Err(failure::request_invalid(
            "validate_resolution_request",
            "entries",
            "field failed validation",
        ));
    }
    let mut seen = HashSet::new();
    for entry in &request.entries {
        if entry.availability != PackageAvailability::Canonical {
            return Err(failure::request_invalid(
                "validate_resolution_request",
                "availability",
                "field failed validation",
            ));
        }
        bounded_nonempty_characters(&entry.manager, PACKAGE_MANAGER_CHARS_MAX, "manager")?;
        bounded_nonempty_characters(&entry.name, PACKAGE_NAME_CHARS_MAX, "name")?;
        if entry.version.is_some() == entry.requirement.is_some() {
            return Err(failure::request_invalid(
                "validate_resolution_request",
                "version_or_requirement",
                "field failed validation",
            ));
        }
        if entry
            .version
            .iter()
            .chain(&entry.requirement)
            .any(|selector| selector.chars().count() > PACKAGE_VERSION_CHARS_MAX)
        {
            return Err(failure::request_invalid(
                "validate_resolution_request",
                "selector",
                "field failed validation",
            ));
        }
        let key = context_key(entry);
        if !seen.insert(key) {
            return Err(failure::request_invalid(
                "validate_resolution_request",
                "duplicate_entry",
                "field failed validation",
            ));
        }
    }
    Ok(())
}

fn validate_resolution_response(
    request: &PackageResolutionRequest,
    response: &PackageResolutionResponse,
) -> Result<(), RiftError> {
    let expected: HashSet<_> = request.entries.iter().map(context_key).collect();
    let mut seen = HashSet::new();
    for package in response
        .available_exact
        .iter()
        .chain(&response.missing_exact)
    {
        validate_package_identity(package).map_err(|source| {
            failure::response_source(
                "validate_resolution_response",
                "package",
                "field failed validation",
                None,
                source,
            )
        })?;
        let key = (
            package.manager.clone(),
            Some(package.registry.clone()),
            package.name.clone(),
            Some(package.version.clone()),
            None,
        );
        if !expected.contains(&key) || !seen.insert(key) {
            return Err(failure::response_invalid(
                "validate_resolution_response",
                "resolution_accounting",
                "field failed validation",
                None,
            ));
        }
    }
    for resolved in &response.resolved_requirements {
        validate_package_identity(&resolved.package).map_err(|source| {
            failure::response_source(
                "validate_resolution_response",
                "package",
                "field failed validation",
                None,
                source,
            )
        })?;
        if !resolved.answers_its_entry() {
            return Err(failure::response_invalid(
                "validate_resolution_response",
                "resolved_requirement",
                "field failed validation",
                None,
            ));
        }
        let key = context_key(&resolved.entry);
        if !expected.contains(&key) || !seen.insert(key) {
            return Err(failure::response_invalid(
                "validate_resolution_response",
                "resolution_accounting",
                "field failed validation",
                None,
            ));
        }
    }
    for entry in &response.missing_requirements {
        if entry.requirement.is_none()
            || entry.version.is_some()
            || entry.availability != PackageAvailability::Canonical
        {
            return Err(failure::response_invalid(
                "validate_resolution_response",
                "missing_requirement",
                "field failed validation",
                None,
            ));
        }
        let key = context_key(entry);
        if !expected.contains(&key) || !seen.insert(key) {
            return Err(failure::response_invalid(
                "validate_resolution_response",
                "resolution_accounting",
                "field failed validation",
                None,
            ));
        }
    }
    if seen.len() != expected.len() {
        return Err(failure::response_invalid(
            "validate_resolution_response",
            "resolution_accounting",
            "field failed validation",
            None,
        ));
    }
    validate_resolution_warnings(response)
}

/// Checks the resolution's warnings: bounded like page warnings, and each
/// `requirement_unsatisfied` naming, through its `detail`, one requirement of
/// `resolved_requirements` beside the version that answers it, at most once.
fn validate_resolution_warnings(response: &PackageResolutionResponse) -> Result<(), RiftError> {
    let warnings = response.warnings.as_deref().unwrap_or_default();
    if warnings.len() > DEPENDENCY_ENTRIES_MAX || !warning_details_within_bound(warnings) {
        return Err(failure::response_invalid(
            "validate_resolution_warnings",
            "warnings",
            "field failed validation",
            None,
        ));
    }
    let named: HashSet<String> = response
        .resolved_requirements
        .iter()
        .filter_map(ResolvedRequirement::unsatisfied_detail)
        .collect();
    let mut seen = HashSet::new();
    for warning in warnings {
        if warning.code != WarningCode::RequirementUnsatisfied {
            continue;
        }
        let detail = warning.detail.as_deref();
        let names_a_requirement = detail.is_some_and(|detail| named.contains(detail));
        if !names_a_requirement || !seen.insert(detail) {
            return Err(failure::response_invalid(
                "validate_resolution_warnings",
                "resolution_warning",
                "field failed validation",
                None,
            ));
        }
    }
    Ok(())
}

/// Whether every warning's `detail`, when present, holds 1 to `WARNING_DETAIL_CHARS_MAX`
/// characters.
fn warning_details_within_bound(warnings: &[Warning]) -> bool {
    warnings.iter().all(|warning| {
        warning
            .detail
            .as_ref()
            .is_none_or(|detail| within_characters(detail, WARNING_DETAIL_CHARS_MAX))
    })
}

impl ResolvedRequirement {
    /// Whether the served package is the entry's package, answering a requirement at any
    /// version or an exact version at another one. An exact entry answered at its own version
    /// belongs in `available_exact`.
    fn answers_its_entry(&self) -> bool {
        let names_the_package = self.package.manager == self.entry.manager
            && self.entry.registry.as_ref() == Some(&self.package.registry)
            && self.package.name == self.entry.name;
        let selector_holds = match (&self.entry.version, &self.entry.requirement) {
            (None, Some(_)) => true,
            (Some(requested), None) => requested != &self.package.version,
            _ => false,
        };
        names_the_package && selector_holds
    }

    /// The `detail` a `requirement_unsatisfied` warning carries for this requirement:
    /// `<manager>/<name> <requirement> answered by <version>`. `None` for an exact entry.
    fn unsatisfied_detail(&self) -> Option<String> {
        let requirement = self.entry.requirement.as_ref()?;
        Some(format!(
            "{}/{} {requirement}{ANSWERED_BY}{}",
            self.entry.manager, self.entry.name, self.package.version
        ))
    }
}

/// One entry the global index answers from a release other than the one it names.
#[derive(Debug, Clone, PartialEq)]
pub struct Substitution {
    /// The entry as the dependency context states it.
    pub requested: PackageContextEntry,
    /// The collected release that answers it.
    pub served: PackageIdentity,
}

impl Capabilities {
    /// Most entries one resolution request carries under these capabilities: the smaller of
    /// the advertised `dependency_entries_max` and [`DEPENDENCY_ENTRIES_MAX`]. The client
    /// refuses a longer request naming `entries` before it reaches the network.
    #[must_use]
    pub fn dependency_entries_max(&self) -> usize {
        smaller_bound(self.bounds.dependency_entries_max, DEPENDENCY_ENTRIES_MAX)
    }
}

impl PackageResolutionResponse {
    /// The substitutions this resolution names, in `resolved_requirements` order: each exact
    /// entry answered at another version, and each requirement whose served version a
    /// `requirement_unsatisfied` warning names in its `detail`.
    ///
    /// The resolution names an exact substitution by answering the exact entry in
    /// `resolved_requirements` beside the served package. An `available_exact` package at a
    /// version no entry requested is an unnamed substitution, and the client refuses that
    /// whole resolution as `resolution_accounting`.
    #[must_use]
    pub fn substitutions(&self) -> Vec<Substitution> {
        let unsatisfied: HashSet<&str> = self
            .warnings
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|warning| warning.code == WarningCode::RequirementUnsatisfied)
            .filter_map(|warning| warning.detail.as_deref())
            .collect();
        self.resolved_requirements
            .iter()
            .filter(|resolved| {
                let exact_elsewhere = resolved
                    .entry
                    .version
                    .as_ref()
                    .is_some_and(|requested| requested != &resolved.package.version);
                let outside_its_range = resolved
                    .unsatisfied_detail()
                    .is_some_and(|detail| unsatisfied.contains(detail.as_str()));
                exact_elsewhere || outside_its_range
            })
            .map(|resolved| Substitution {
                requested: resolved.entry.clone(),
                served: resolved.package.clone(),
            })
            .collect()
    }
}

fn validate_search_request(request: &PackageSearchRequest) -> Result<(), RiftError> {
    if let Some(query) = request.query.as_deref() {
        validate_search_lexical_request(request, query)?;
    } else if request.traversal.is_none()
        || request.terms.is_some()
        || request.identifiers.is_some()
        || request.phase.is_some()
    {
        return Err(failure::request_invalid(
            "validate_search_request",
            "query",
            "field failed validation",
        ));
    }
    validate_search_traversal_request(request)?;
    if let Some(view) = request.view.as_deref() {
        rift_protocol::symbol_read::CapturedViewId::parse(view).map_err(|source| {
            failure::request_invalid_source(
                "validate_search_request",
                "view",
                "field failed validation",
                source,
            )
        })?;
    }
    if request
        .include
        .as_ref()
        .is_some_and(|values| values.len() > 1 || values.iter().any(|value| value != "source"))
    {
        return Err(failure::request_invalid(
            "validate_search_request",
            "include",
            "field failed validation",
        ));
    }
    validate_packages(request.packages.as_deref().unwrap_or_default())
}

fn validate_search_lexical_request(
    request: &PackageSearchRequest,
    query: &str,
) -> Result<(), RiftError> {
    bounded_nonempty_characters(query, QUERY_BYTES_MAX, "query")?;
    let terms = request.terms.as_deref().ok_or_else(|| {
        failure::request_invalid(
            "validate_search_request",
            "terms",
            "field failed validation",
        )
    })?;
    let identifiers = request.identifiers.as_deref().ok_or_else(|| {
        failure::request_invalid(
            "validate_search_request",
            "identifiers",
            "field failed validation",
        )
    })?;
    let phase = request.phase.as_ref().ok_or_else(|| {
        failure::request_invalid(
            "validate_search_request",
            "phase",
            "field failed validation",
        )
    })?;
    if terms.is_empty() || terms.len() > QUERY_TERMS_MAX {
        return Err(failure::request_invalid(
            "validate_search_request",
            "terms",
            "field failed validation",
        ));
    }
    for term in terms {
        bounded_nonempty_characters(&term.text, QUERY_TERM_BYTES_MAX, "term")?;
        if term.phrase && term.prefix {
            return Err(failure::request_invalid(
                "validate_search_request",
                "phrase_prefix",
                "field failed validation",
            ));
        }
        if term.prefix
            && term
                .text
                .chars()
                .filter(|character| character.is_alphanumeric())
                .count()
                < 3
        {
            return Err(failure::request_invalid(
                "validate_search_request",
                "prefix",
                "field failed validation",
            ));
        }
    }
    if identifiers.len() > IDENTIFIERS_MAX
        || identifiers
            .iter()
            .any(|value| value.is_empty() || value.len() > IDENTIFIER_BYTES_MAX)
    {
        return Err(failure::request_invalid(
            "validate_search_request",
            "identifiers",
            "field failed validation",
        ));
    }
    if matches!(phase, PackageSearchRequestPhase::Broad)
        && terms.iter().filter(|term| !term.phrase).count() < 2
    {
        return Err(failure::request_invalid(
            "validate_search_request",
            "broad_phase",
            "field failed validation",
        ));
    }
    Ok(())
}

fn validate_search_traversal_request(request: &PackageSearchRequest) -> Result<(), RiftError> {
    let Some(traversal) = &request.traversal else {
        return Ok(());
    };
    if request
        .target
        .as_ref()
        .is_some_and(|target| target != &PackageSearchRequestTarget::Symbol)
        || !(1..=rift_protocol::search::SEARCH_TRAVERSAL_DEPTH_MAX).contains(
            &traversal
                .depth
                .unwrap_or(rift_protocol::search::SEARCH_TRAVERSAL_DEPTH_DEFAULT),
        )
        || traversal
            .facets
            .as_ref()
            .is_some_and(|facets| facets.len() > rift_protocol::search::SEARCH_TRAVERSAL_FACETS_MAX)
    {
        return Err(failure::request_invalid(
            "validate_search_request",
            "traversal",
            "field failed validation",
        ));
    }
    let seed = traversal.seed.as_deref().ok_or_else(|| {
        failure::request_invalid(
            "validate_search_request",
            "traversal.seed",
            "field failed validation",
        )
    })?;
    for (field, value) in [
        ("traversal.seed", Some(seed)),
        ("traversal.to", traversal.to.as_deref()),
    ] {
        let Some(value) = value else { continue };
        let identity = rift_protocol::identity::SymbolIdentity::parse(value).map_err(|source| {
            failure::request_invalid_source(
                "validate_search_request",
                field,
                "field failed validation",
                source,
            )
        })?;
        if !matches!(
            identity.owner(),
            rift_protocol::identity::SymbolOwner::Package { .. }
                | rift_protocol::identity::SymbolOwner::Runtime { .. }
        ) {
            return Err(failure::request_invalid(
                "validate_search_request",
                field,
                "field failed validation",
            ));
        }
    }
    Ok(())
}

/// Checks contract character bounds and the validated exact defining registry owner.
fn validate_package_identity(package: &PackageIdentity) -> Result<(), RiftError> {
    bounded_nonempty_characters(
        &package.manager,
        PACKAGE_MANAGER_CHARS_MAX,
        "package_manager",
    )?;
    bounded_nonempty_characters(&package.name, PACKAGE_NAME_CHARS_MAX, "package_name")?;
    bounded_nonempty_characters(&package.registry, PACKAGE_REGISTRY_CHARS_MAX, "registry")?;
    bounded_nonempty_characters(
        &package.version,
        PACKAGE_VERSION_CHARS_MAX,
        "package_version",
    )?;
    domain::package_identity(package)
        .owner()
        .map(|_| ())
        .map_err(|source| {
            failure::request_invalid_source(
                "validate_package_identity",
                "package",
                "field failed validation",
                source,
            )
        })
}

fn validate_packages(packages: &[PackageIdentity]) -> Result<(), RiftError> {
    if packages.len() > PACKAGES_MAX {
        return Err(failure::request_invalid(
            "validate_packages",
            "packages",
            "field failed validation",
        ));
    }
    let mut seen = HashSet::new();
    for package in packages {
        validate_package_identity(package)?;
        if !seen.insert((
            package.manager.as_str(),
            package.registry.as_str(),
            package.name.as_str(),
            package.version.as_str(),
        )) {
            return Err(failure::request_invalid(
                "validate_packages",
                "duplicate_package",
                "field failed validation",
            ));
        }
    }
    Ok(())
}

/// The page size one paged read asks for: the caller's limit, cut to the advertised
/// `page_limit_max`.
fn advertised_page_limit(limit: i64, capabilities: &Capabilities) -> i64 {
    limit.min(capabilities.bounds.page_limit_max)
}

fn validate_page(limit: i64, cursor: Option<&str>) -> Result<(), RiftError> {
    if !(PAGE_LIMIT_MIN..=PAGE_LIMIT_MAX).contains(&limit) {
        return Err(failure::request_invalid(
            "validate_page",
            "limit",
            "field failed validation",
        ));
    }
    if cursor.is_some_and(|value| value.is_empty() || value.len() > CURSOR_BYTES_MAX) {
        return Err(failure::request_invalid(
            "validate_page",
            "cursor",
            "field failed validation",
        ));
    }
    Ok(())
}

fn validate_search_page_metadata(
    request: &PackageSearchRequest,
    capabilities: &Capabilities,
    page: &PackageSearchPage,
) -> Result<bool, RiftError> {
    chrono::DateTime::parse_from_rfc3339(&page.view.expires_at).map_err(|source| {
        failure::response_source(
            "validate_search_page",
            "view.expires_at",
            "invalid captured view expiry",
            None,
            source,
        )
    })?;
    if request
        .view
        .as_deref()
        .is_some_and(|view| view != page.view.id.as_str())
    {
        return Err(failure::response_invalid(
            "validate_search_page",
            "view",
            "captured view does not match request",
            None,
        ));
    }
    let target = request
        .target
        .as_ref()
        .unwrap_or(&PackageSearchRequestTarget::Symbol);
    let documentation_requested = matches!(
        target,
        PackageSearchRequestTarget::Documentation | PackageSearchRequestTarget::All
    );
    PageMetadata {
        publication_format: &page.publication_format,
        analyzer_revision: &page.analyzer_revision,
        corpus_revision: &page.corpus_revision,
        documentation_revision: page.documentation_revision.as_deref(),
        next_cursor: page.next_cursor.as_deref(),
        warnings: &page.warnings,
    }
    .validate(capabilities, documentation_requested)?;
    Ok(documentation_requested)
}

fn validate_search_documentation_hit(
    request: &PackageSearchRequest,
    capabilities: &Capabilities,
    hit: &generated::PackageDocumentationHit,
    selection: Option<&HashSet<(String, String, String, String)>>,
    source_bytes_max: usize,
    documentation_requested: bool,
    documentation_bytes: &mut usize,
) -> Result<(), RiftError> {
    validate_package_identity(&hit.package).map_err(|source| {
        failure::response_source(
            "validate_search_page",
            "package",
            "field failed validation",
            None,
            source,
        )
    })?;
    if hit.source.is_some() && !includes_source(request.include.as_deref()) {
        return Err(failure::response_invalid(
            "validate_search_page",
            "source",
            "field failed validation",
            None,
        ));
    }
    *documentation_bytes += hit.source.as_ref().map_or(0, String::len);
    validate_documentation_source_bytes(
        hit.source.as_ref().map_or(0, String::len),
        smaller_bound(capabilities.bounds.source_bytes_max, source_bytes_max),
    )?;
    validate_documentation_bytes(*documentation_bytes)?;
    if !documentation_requested
        || !supports_feature(capabilities, DOCUMENTATION_SEARCH_FEATURE)
        || selection.is_some_and(|packages| !packages.contains(&package_key(&hit.package)))
    {
        return Err(failure::response_invalid(
            "validate_search_page",
            "documentation",
            "field failed validation",
            None,
        ));
    }
    if hit.contributing_fields.is_empty() {
        return Err(failure::response_invalid(
            "validate_search_page",
            "contributing_fields",
            "field failed validation",
            None,
        ));
    }
    PackageSearchCandidate::try_from(PackageSearchItem::Documentation(Box::new(hit.clone())))?;
    Ok(())
}

fn validate_search_page(
    request: &PackageSearchRequest,
    capabilities: &Capabilities,
    page: &PackageSearchPage,
    cursor: Option<&str>,
    source_bytes_max: usize,
) -> Result<(), RiftError> {
    let documentation_requested = validate_search_page_metadata(request, capabilities, page)?;
    let target = request
        .target
        .as_ref()
        .unwrap_or(&PackageSearchRequestTarget::Symbol);
    let packages: HashSet<_> = request
        .packages
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(package_key)
        .collect();
    let selection = (!packages.is_empty()).then_some(&packages);
    let mut seen = HashSet::new();
    let mut documentation_bytes = 0_usize;
    for hit in &page.items {
        match hit {
            PackageSearchItem::Search(hit) => {
                if target == &PackageSearchRequestTarget::Documentation {
                    return Err(failure::response_invalid(
                        "validate_search_page",
                        "target",
                        "field failed validation",
                        None,
                    ));
                }
                if hit.source.is_some() && !includes_source(request.include.as_deref()) {
                    return Err(failure::response_invalid(
                        "validate_search_page",
                        "source",
                        "field failed validation",
                        None,
                    ));
                }
                let qualified_name = validate_hit_common(
                    &hit.package,
                    &hit.symbol,
                    HitLocation {
                        unit: &hit.unit,
                        range: &hit.range,
                        line: hit.line,
                        source: hit.source.as_deref(),
                    },
                    selection,
                    smaller_bound(capabilities.bounds.source_bytes_max, source_bytes_max),
                )?;
                validate_search_match_class(request, hit, &qualified_name)?;
            }
            PackageSearchItem::Documentation(hit) => validate_search_documentation_hit(
                request,
                capabilities,
                hit,
                selection,
                source_bytes_max,
                documentation_requested,
                &mut documentation_bytes,
            )?,
            PackageSearchItem::Symbol(hit) => {
                validate_canonical_search_hit(
                    request,
                    capabilities,
                    hit,
                    selection,
                    source_bytes_max,
                )?;
            }
        }
        let key = search_hit_identity(hit)?;
        if !seen.insert(key) {
            return Err(failure::response_invalid(
                "validate_search_page",
                "duplicate_item",
                "field failed validation",
                None,
            ));
        }
    }
    if cursor.is_some() && page.items.is_empty() && page.next_cursor.as_deref() == cursor {
        return Err(failure::response_invalid(
            "validate_search_page",
            "cursor_progress",
            "field failed validation",
            None,
        ));
    }
    Ok(())
}

fn validate_canonical_search_hit(
    request: &PackageSearchRequest,
    capabilities: &Capabilities,
    hit: &rift_protocol::read::SearchHit,
    packages: Option<&HashSet<(String, String, String, String)>>,
    source_bytes_max: usize,
) -> Result<(), RiftError> {
    let candidate =
        PackageSearchCandidate::try_from(PackageSearchItem::Symbol(Box::new(hit.clone())))?;
    if request.target.as_ref() == Some(&PackageSearchRequestTarget::Documentation) {
        return Err(failure::response_invalid(
            "validate_search_page",
            "target",
            "field failed validation",
            None,
        ));
    }
    if let Some(packages) = packages {
        let admitted = candidate.package.as_ref().is_some_and(|package| {
            packages.contains(&(
                package.manager.clone(),
                package.registry.clone(),
                package.name.clone(),
                package.version.clone(),
            ))
        });
        if !admitted {
            return Err(failure::response_invalid(
                "validate_search_page",
                "package",
                "field failed validation",
                None,
            ));
        }
    }
    if hit.source.as_ref().is_some_and(|source| {
        !includes_source(request.include.as_deref())
            || source.len() > smaller_bound(capabilities.bounds.source_bytes_max, source_bytes_max)
    }) {
        return Err(failure::response_invalid(
            "validate_search_page",
            "source",
            "field failed validation",
            None,
        ));
    }
    if hit.range.is_some() != hit.line.is_some()
        || hit
            .range
            .as_ref()
            .is_some_and(|range| range.end < range.start)
        || hit.line == Some(0)
        || hit.source.is_some() && hit.range.is_none()
    {
        return Err(failure::response_invalid(
            "validate_search_page",
            "location",
            "field failed validation",
            None,
        ));
    }
    let distance = hit
        .traversal_path
        .as_ref()
        .map(|path| u64::try_from(path.len()))
        .transpose()
        .map_err(|source| {
            failure::response_source(
                "validate_search_page",
                "traversal_path",
                "field failed validation",
                None,
                source,
            )
        })?;
    if hit.distance != distance
        || distance
            .is_some_and(|distance| distance > rift_protocol::search::SEARCH_TRAVERSAL_DEPTH_MAX)
    {
        return Err(failure::response_invalid(
            "validate_search_page",
            "traversal_path",
            "field failed validation",
            None,
        ));
    }
    Ok(())
}

fn validate_documentation_source_bytes(bytes: usize, maximum: usize) -> Result<(), RiftError> {
    if bytes > maximum {
        return Err(failure::response_invalid(
            "validate_documentation_source_bytes",
            "source",
            "field failed validation",
            None,
        ));
    }
    Ok(())
}

fn validate_documentation_bytes(bytes: usize) -> Result<(), RiftError> {
    if bytes > rift_protocol::documentation::DOCUMENTATION_EXCERPT_BYTES_MAX as usize {
        return Err(failure::response_invalid(
            "validate_documentation_bytes",
            "source",
            "field failed validation",
            None,
        ));
    }
    Ok(())
}

struct PageMetadata<'a> {
    publication_format: &'a PublicationFormat,
    analyzer_revision: &'a str,
    corpus_revision: &'a str,
    documentation_revision: Option<&'a str>,
    next_cursor: Option<&'a str>,
    warnings: &'a [Warning],
}

impl PageMetadata<'_> {
    fn validate(
        &self,
        capabilities: &Capabilities,
        documentation_required: bool,
    ) -> Result<(), RiftError> {
        if self.publication_format != &capabilities.publication_format {
            return Err(failure::response_invalid(
                "validate",
                "publication_format",
                "field failed validation",
                None,
            ));
        }
        if !within_characters(self.analyzer_revision, LABEL_CHARS_MAX)
            || self.corpus_revision != capabilities.corpus_revision
        {
            return Err(failure::response_invalid(
                "validate",
                "revision",
                "field failed validation",
                None,
            ));
        }
        match (
            self.documentation_revision,
            capabilities.documentation_revision.as_deref(),
        ) {
            (Some(actual), Some(expected))
                if actual == expected && valid_revision_digest(actual) => {}
            (None, _) if !documentation_required => {}
            _ => {
                return Err(failure::response_invalid(
                    "validate",
                    "documentation_revision",
                    "field failed validation",
                    None,
                ));
            }
        }
        if self.next_cursor.is_some_and(|value| {
            value.is_empty()
                || value.len()
                    > smaller_bound(capabilities.bounds.cursor_bytes_max, CURSOR_BYTES_MAX)
        }) {
            return Err(failure::response_invalid(
                "validate",
                "cursor",
                "field failed validation",
                None,
            ));
        }
        if self.warnings.len() > smaller_bound(capabilities.bounds.warnings_max, WARNINGS_MAX)
            || !warning_details_within_bound(self.warnings)
        {
            return Err(failure::response_invalid(
                "validate",
                "warnings",
                "field failed validation",
                None,
            ));
        }
        Ok(())
    }
}

fn includes_source(include: Option<&[String]>) -> bool {
    include.is_some_and(|fields| fields.iter().any(|field| field == "source"))
}

#[derive(Clone, Copy)]
struct HitLocation<'a> {
    unit: &'a str,
    range: &'a TextRange,
    line: i64,
    source: Option<&'a str>,
}

fn validate_hit_common(
    package: &PackageIdentity,
    symbol: &Symbol,
    location: HitLocation<'_>,
    packages: Option<&HashSet<(String, String, String, String)>>,
    source_bytes_max: usize,
) -> Result<String, RiftError> {
    let source_path = validate_hit_fields(package, symbol, location, packages, source_bytes_max)?;
    validate_symbol_identity(symbol, package, &source_path)
}

fn validate_canonical_hit(
    package: &PackageIdentity,
    symbol: &Symbol,
    location: HitLocation<'_>,
    packages: Option<&HashSet<(String, String, String, String)>>,
    source_bytes_max: usize,
) -> Result<(), RiftError> {
    validate_hit_fields(package, symbol, location, packages, source_bytes_max)?;
    let invalid = || {
        failure::response_invalid(
            "validate_canonical_hit",
            "symbol_identity",
            "field failed validation",
            None,
        )
    };
    let id = symbol.id.as_deref().ok_or_else(invalid)?;
    let identity = rift_protocol::identity::SymbolIdentity::parse(id).map_err(|source| {
        failure::response_source(
            "validate_canonical_hit",
            "symbol_identity",
            "field failed validation",
            None,
            source,
        )
    })?;
    let owner = domain::package_identity(package)
        .owner()
        .map_err(|_| invalid())?;
    let language = rift_protocol::read::Language::from_identity_segment(&symbol.language)
        .map_err(|_| invalid())?;
    if identity.owner() != &owner || identity.language() != &language {
        return Err(invalid());
    }
    Ok(())
}

fn validate_hit_fields(
    package: &PackageIdentity,
    symbol: &Symbol,
    location: HitLocation<'_>,
    packages: Option<&HashSet<(String, String, String, String)>>,
    source_bytes_max: usize,
) -> Result<String, RiftError> {
    validate_package_identity(package).map_err(|source| {
        failure::response_source(
            "validate_hit_common",
            "package",
            "field failed validation",
            None,
            source,
        )
    })?;
    if packages.is_some_and(|packages| !packages.contains(&package_key(package))) {
        return Err(failure::response_invalid(
            "validate_hit_common",
            "package",
            "field failed validation",
            None,
        ));
    }
    if location.line < 1 || location.range.end < location.range.start {
        return Err(failure::response_invalid(
            "validate_hit_common",
            "location",
            "field failed validation",
            None,
        ));
    }
    if location
        .source
        .is_some_and(|value| value.len() > source_bytes_max)
    {
        return Err(failure::response_invalid(
            "validate_hit_common",
            "source",
            "field failed validation",
            None,
        ));
    }
    let source_path = package_source_path(package, location.unit)?;
    let Some(origin) = symbol.origin.as_ref() else {
        return Err(failure::response_invalid(
            "validate_hit_common",
            "origin",
            "field failed validation",
            None,
        ));
    };
    if origin.location != Some(SourceLocationKind::Dependency)
        || origin.package.as_ref() != Some(package)
    {
        return Err(failure::response_invalid(
            "validate_hit_common",
            "origin",
            "field failed validation",
            None,
        ));
    }
    Ok(source_path)
}

/// The package-relative path of `unit`, after its exact defining registry owner is validated.
fn package_source_path(package: &PackageIdentity, unit: &str) -> Result<String, RiftError> {
    let invalid = failure::response_invalid(
        "package_source_path",
        "source_identity",
        "field failed validation",
        None,
    );
    let unit = rift_core::SourceUnitId::parse(unit).map_err(|source| {
        failure::response_source(
            "package_source_path",
            "source_identity",
            "field failed validation",
            None,
            source,
        )
    })?;
    let owner = domain::package_identity(package)
        .owner()
        .map_err(|source| {
            failure::response_source(
                "package_source_path",
                "source_identity",
                "field failed validation",
                None,
                source,
            )
        })?;
    if unit.source_owner() != Some(&owner) {
        return Err(invalid);
    }
    Ok(unit.key().as_str().to_owned())
}

/// Checks that a package hit's symbol identity is the one its unit mints, and returns its
/// qualified name.
///
/// Package analysis mints a package declaration's identity over its unit's resolver and key,
/// `<manager>/<name>@<version>/<path>`, as `rift://symbol/rust/cargo/beacon@1.0.0/src/lib.rs/serve`
/// for `rift://source/cargo/beacon@1.0.0/src/lib.rs`. No producer mints the spelling over the
/// package-relative path alone, `rift://symbol/rust/src/lib.rs/serve`, for a package
/// declaration: it names a project file, and the client refuses it.
fn validate_symbol_identity(
    symbol: &Symbol,
    package: &PackageIdentity,
    source_path: &str,
) -> Result<String, RiftError> {
    let id = symbol.id.as_deref().ok_or(failure::response_invalid(
        "validate_symbol_identity",
        "symbol_identity",
        "field failed validation",
        None,
    ))?;
    if id.len() > 8_192 || rift_core::SymbolId::new(id).is_err() {
        return Err(failure::response_invalid(
            "validate_symbol_identity",
            "symbol_identity",
            "field failed validation",
            None,
        ));
    }
    let address = id
        .strip_prefix("rift://symbol/")
        .ok_or(failure::response_invalid(
            "validate_symbol_identity",
            "symbol_identity",
            "field failed validation",
            None,
        ))?;
    let (language, remainder) = address.split_once('/').ok_or(failure::response_invalid(
        "validate_symbol_identity",
        "symbol_identity",
        "field failed validation",
        None,
    ))?;
    if language != symbol.language
        || rift_protocol::read::Language::from_identity_segment(language).is_err()
    {
        return Err(failure::response_invalid(
            "validate_symbol_identity",
            "symbol_identity",
            "field failed validation",
            None,
        ));
    }
    let (_, encoded_qualified_name) =
        remainder.rsplit_once('/').ok_or(failure::response_invalid(
            "validate_symbol_identity",
            "symbol_identity",
            "field failed validation",
            None,
        ))?;
    let qualified_name = percent_decode_str(encoded_qualified_name)
        .decode_utf8()
        .map_err(|source| {
            failure::response_source(
                "validate_symbol_identity",
                "symbol_identity",
                "field failed validation",
                None,
                source,
            )
        })?;
    let unit_path = format!(
        "{}/{}@{}/{source_path}",
        package.manager, package.name, package.version
    );
    let minted = rift_core::symbol_identity(language, &unit_path, &qualified_name) == id;
    if qualified_name.is_empty() || !minted {
        return Err(failure::response_invalid(
            "validate_symbol_identity",
            "symbol_identity",
            "field failed validation",
            None,
        ));
    }
    Ok(qualified_name.into_owned())
}

/// Checks a search hit's class against the request's identifiers. A hit reporting `unknown`,
/// found by its text or its vector, matches none of them; a hit claiming an identifier class
/// carries the best match `rift_ranking::identifier_match` gives it.
fn validate_search_match_class(
    request: &PackageSearchRequest,
    hit: &PackageSearchHit,
    qualified_name: &str,
) -> Result<(), RiftError> {
    let claimed = search_match_class(&hit.match_class)?;
    let expected = request
        .identifiers
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|candidate| {
            rift_ranking::identifier_match(candidate, &hit.symbol.name, qualified_name)
        })
        .min()
        .map(rift_ranking::IdentifierMatch::class);
    if expected != claimed {
        return Err(failure::response_invalid(
            "validate_search_match_class",
            "match_class",
            "field failed validation",
            None,
        ));
    }
    Ok(())
}

/// The identifier match class one search hit claims: `None` for `unknown`, which a hit
/// matching none of the requested identifiers reports, such as a declaration matched by
/// its body alone.
fn search_match_class(
    value: &IdentifierMatchClass,
) -> Result<Option<rift_ranking::IdentifierMatchClass>, RiftError> {
    match value {
        IdentifierMatchClass::Unknown => Ok(None),
        class => ranking_match_class(class).map(Some),
    }
}

fn ranking_match_class(
    value: &IdentifierMatchClass,
) -> Result<rift_ranking::IdentifierMatchClass, RiftError> {
    match value {
        IdentifierMatchClass::QualifiedExact => {
            Ok(rift_ranking::IdentifierMatchClass::QualifiedExact)
        }
        IdentifierMatchClass::NameExact => Ok(rift_ranking::IdentifierMatchClass::NameExact),
        IdentifierMatchClass::NamePrefix => Ok(rift_ranking::IdentifierMatchClass::NamePrefix),
        IdentifierMatchClass::Substring => Ok(rift_ranking::IdentifierMatchClass::Substring),
        IdentifierMatchClass::Unknown => Err(failure::response_invalid(
            "ranking_match_class",
            "match_class",
            "field failed validation",
            None,
        )),
    }
}

/// Refuses `value` naming `field` unless it holds 1 to `max` characters, the unit a
/// contract `maxLength` counts.
fn bounded_nonempty_characters(
    value: &str,
    max: usize,
    field: &'static str,
) -> Result<(), RiftError> {
    if within_characters(value, max) {
        Ok(())
    } else {
        Err(failure::request_invalid(
            "bounded_nonempty_characters",
            field,
            "field failed validation",
        ))
    }
}

/// Whether `value` holds 1 to `max` characters.
fn within_characters(value: &str, max: usize) -> bool {
    !value.is_empty() && value.chars().count() <= max
}

fn package_key(package: &PackageIdentity) -> (String, String, String, String) {
    (
        package.manager.clone(),
        package.registry.clone(),
        package.name.clone(),
        package.version.clone(),
    )
}

fn context_key(
    entry: &PackageContextEntry,
) -> (
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
) {
    (
        entry.manager.clone(),
        entry.registry.clone(),
        entry.name.clone(),
        entry.version.clone(),
        entry.requirement.clone(),
    )
}

fn bounded_candidate_pool(capabilities: &Capabilities) -> usize {
    smaller_bound(capabilities.bounds.candidate_pool_max, CANDIDATE_POOL_MAX)
}

fn smaller_bound(value: i64, local: usize) -> usize {
    usize::try_from(value).map_or(local, |value| value.min(local))
}

fn positive_within(value: i64, local: usize) -> bool {
    value > 0 && usize::try_from(value).is_ok_and(|value| value <= local)
}

fn validate_assembled_revision(
    revisions: &mut Option<(String, String, Option<String>)>,
    analyzer_revision: &str,
    corpus_revision: &str,
    documentation_revision: Option<&str>,
) -> Result<(), RiftError> {
    match revisions {
        Some((analyzer, corpus, documentation))
            if analyzer != analyzer_revision
                || corpus != corpus_revision
                || documentation.as_deref() != documentation_revision =>
        {
            Err(failure::response_invalid(
                "validate_assembled_revision",
                "revision",
                "field failed validation",
                None,
            ))
        }
        Some(_) => Ok(()),
        None => {
            *revisions = Some((
                analyzer_revision.to_owned(),
                corpus_revision.to_owned(),
                documentation_revision.map(str::to_owned),
            ));
            Ok(())
        }
    }
}

fn extend_warnings(target: &mut Vec<Warning>, warnings: Vec<Warning>) {
    for warning in warnings {
        if target.len() == WARNINGS_MAX {
            return;
        }
        if !target.contains(&warning) {
            target.push(warning);
        }
    }
}

fn assembled_revisions(
    revisions: Option<(String, String, Option<String>)>,
) -> (String, String, Option<String>) {
    revisions.unwrap_or_default()
}

/// Whether a page ended short of `limit` at the response body bound. The server marks such a
/// page with `result_truncated` beside a cursor at its first left-out item; a page carrying
/// `limit` items ended at `limit`, whatever it warns.
fn stopped_at_body_bound(warnings: &[Warning], item_count: usize, limit: i64) -> bool {
    let short = i64::try_from(item_count).is_ok_and(|count| count < limit);
    let marked = warnings
        .iter()
        .any(|warning| warning.code == WarningCode::ResultTruncated);
    short && marked
}

fn search_pages(
    items: Vec<PackageSearchItem>,
    warnings: Vec<Warning>,
    revisions: Option<(String, String, Option<String>)>,
    view: Option<rift_protocol::symbol_read::CapturedView>,
    next_cursor: Option<String>,
) -> Result<PackageSearchPages, RiftError> {
    let (analyzer_revision, corpus_revision, documentation_revision) =
        assembled_revisions(revisions);
    Ok(PackageSearchPages {
        items,
        warnings,
        analyzer_revision,
        corpus_revision,
        documentation_revision,
        view: view.ok_or_else(|| {
            failure::response_invalid("search_pages", "view", "captured view is absent", None)
        })?,
        next_cursor,
    })
}

fn search_hit_identity(hit: &PackageSearchItem) -> Result<String, RiftError> {
    match hit {
        PackageSearchItem::Search(hit) => {
            if let Some(id) = hit.symbol.id.as_deref() {
                return rift_protocol::identity::SymbolIdentity::parse(id)
                    .map(|identity| identity.wire_identity())
                    .map_err(|source| {
                        failure::response_source(
                            "search_hit_identity",
                            "symbol.id",
                            "field failed validation",
                            None,
                            source,
                        )
                    });
            }
            serde_json::to_string(&(package_key(&hit.package), &hit.unit, &hit.symbol.name))
                .map_err(|source| {
                    failure::response_source(
                        "search_hit_identity",
                        "symbol_identity",
                        "field failed validation",
                        None,
                        source,
                    )
                })
        }
        PackageSearchItem::Documentation(hit) => {
            let identity = rift_ranking::DocumentIdentity::for_documentation_block(
                &hit.documentation.block.identity,
            )
            .map_err(|source| {
                failure::response_source(
                    "search_hit_identity",
                    "documentation.identity",
                    "field failed validation",
                    None,
                    source,
                )
            })?;
            Ok(identity.as_str().to_owned())
        }
        PackageSearchItem::Symbol(hit) => {
            let candidate =
                PackageSearchCandidate::try_from(PackageSearchItem::Symbol(hit.clone()))?;
            Ok(candidate.identity.as_str().to_owned())
        }
    }
}

fn validate_config(config: &Config) -> Result<(), RiftError> {
    for (field, value, maximum) in [
        (
            "max_request",
            config.max_request,
            rift_protocol::configuration::GLOBAL_REQUEST_BYTES_MAX,
        ),
        (
            "max_response",
            config.max_response,
            rift_protocol::configuration::GLOBAL_RESPONSE_BYTES_MAX,
        ),
        (
            "max_source",
            config.max_source,
            rift_protocol::configuration::GLOBAL_SOURCE_BYTES_MAX,
        ),
    ] {
        if value == 0 || u64::try_from(value).map_or(true, |value| value > maximum) {
            return Err(failure::configuration_invalid(
                field,
                "outside configured bounds",
            ));
        }
    }
    if !(Duration::from_millis(100)..=Duration::from_secs(30)).contains(&config.connect_timeout) {
        return Err(failure::configuration_invalid(
            "connect_timeout",
            "outside configured bounds",
        ));
    }
    if !(Duration::from_secs(1)..=Duration::from_mins(5)).contains(&config.request_timeout) {
        return Err(failure::configuration_invalid(
            "request_timeout",
            "outside configured bounds",
        ));
    }
    if !(ATTEMPTS_MIN..=ATTEMPTS_MAX).contains(&config.attempts) {
        return Err(failure::configuration_invalid(
            "attempts",
            "outside configured bounds",
        ));
    }
    if !(1..=32).contains(&config.max_in_flight) {
        return Err(failure::configuration_invalid(
            "max_in_flight",
            "outside configured bounds",
        ));
    }
    if !(Duration::from_mins(1)..=Duration::from_hours(24)).contains(&config.capabilities_ttl) {
        return Err(failure::configuration_invalid(
            "capabilities_ttl",
            "outside configured bounds",
        ));
    }
    if !(Duration::from_mins(1)..=Duration::from_hours(24)).contains(&config.resolution_ttl) {
        return Err(failure::configuration_invalid(
            "resolution_ttl",
            "outside configured bounds",
        ));
    }
    if !(Duration::from_secs(1)..=Duration::from_hours(1)).contains(&config.failure_ttl) {
        return Err(failure::configuration_invalid(
            "failure_ttl",
            "outside configured bounds",
        ));
    }
    if config.token_env.is_empty()
        || config
            .token_env
            .bytes()
            .any(|byte| !(byte == b'_' || byte.is_ascii_alphanumeric()))
    {
        return Err(failure::configuration_invalid(
            "token_env",
            "token environment variable name is invalid",
        ));
    }
    Ok(())
}

fn validate_endpoint(value: &str) -> Result<Url, RiftError> {
    let endpoint = Url::parse(value).map_err(|error| {
        failure::configuration_source(
            "endpoint",
            "endpoint must be an absolute base URL without user information, query, fragment, or trailing slash",
            error,
        )
    })?;
    if endpoint.cannot_be_a_base()
        || endpoint.username() != ""
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.path().ends_with('/')
        || endpoint.host_str().is_none()
    {
        return Err(failure::configuration_invalid(
            "endpoint",
            "endpoint must be an absolute base URL without user information, query, fragment, or trailing slash",
        ));
    }
    if endpoint.scheme() != "https"
        && !(endpoint.scheme() == "http" && endpoint.host_str().is_some_and(is_loopback))
    {
        return Err(failure::configuration_invalid(
            "endpoint",
            "endpoint must use HTTPS except for loopback HTTP",
        ));
    }
    Ok(endpoint)
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

fn read_token(name: &str) -> Result<Option<String>, RiftError> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(failure::configuration_invalid(
            "token",
            "token cannot form a bounded authorization header",
        )),
        Ok(value) => Ok(Some(value)),
    }
}

fn validate_token(value: String) -> Result<String, RiftError> {
    if value.is_empty()
        || value.len() > 8 * 1024
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(failure::configuration_invalid(
            "token",
            "token cannot form a bounded authorization header",
        ));
    }
    Ok(value)
}

fn make_url(endpoint: &Url, path: &str) -> Url {
    let mut url = endpoint.clone();
    url.set_path(&format!("{}{}", endpoint.path(), path));
    url
}

fn serialize_body<T: Serialize>(
    value: &T,
    request_body_bytes_max: usize,
) -> Result<Vec<u8>, RiftError> {
    let body = serde_json::to_vec(value).map_err(|error| {
        failure::request_invalid_source(
            "serialize_body",
            "body",
            "JSON serialization failed",
            error,
        )
    })?;
    if body.len() > request_body_bytes_max {
        return Err(failure::body_limit(
            "serialize_body",
            "request",
            body.len(),
            request_body_bytes_max,
        ));
    }
    Ok(body)
}

fn validate_body_for_capabilities(
    body: &[u8],
    capabilities: &Capabilities,
    request_body_bytes_max: usize,
) -> Result<(), RiftError> {
    let maximum = smaller_bound(
        capabilities.bounds.request_body_bytes_max,
        request_body_bytes_max,
    );
    if body.len() > maximum {
        return Err(failure::body_limit(
            "validate_body_for_capabilities",
            "request",
            body.len(),
            maximum,
        ));
    }
    Ok(())
}

fn header_text(
    headers: &HeaderMap,
    name: impl reqwest::header::AsHeaderName,
    field: &'static str,
) -> Result<Option<String>, RiftError> {
    let value = headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if value
        .as_ref()
        .is_some_and(|value| value.len() > RESPONSE_HEADER_BYTES_MAX)
    {
        return Err(failure::response_invalid(
            "header_text",
            field,
            "field failed validation",
            None,
        ));
    }
    Ok(value)
}
fn is_media(meta: &ResponseMeta, expected: &str) -> bool {
    meta.content_type
        .as_deref()
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
}
fn terminal_search_outcome(operation: contract::Endpoint, response: &RawResponse) -> bool {
    if operation != contract::Endpoint::Search || response.status != StatusCode::SERVICE_UNAVAILABLE
    {
        return false;
    }
    if is_media(&response.meta, "application/json") {
        return serde_json::from_slice::<rift_protocol::search_read::SearchUnavailable>(
            &response.body,
        )
        .is_ok();
    }
    is_media(&response.meta, "application/problem+json")
        && serde_json::from_slice::<ProblemDetails>(&response.body).is_ok_and(|problem| {
            failure::unsupported_operation(operation.operation_id(), &response.meta, Some(&problem))
        })
}

fn should_retry(status: StatusCode, attempt: u32, attempts: u32) -> bool {
    attempt < attempts
        && (status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::SERVICE_UNAVAILABLE
            || (status == StatusCode::GATEWAY_TIMEOUT && attempt == 1))
}
fn retry_delay(meta: &ResponseMeta) -> Option<Duration> {
    meta.retry_after
        .as_deref()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .filter(|delay| *delay <= Duration::from_hours(1))
}
fn bounded_capabilities_ttl(config: &Config, meta: &ResponseMeta) -> Duration {
    let server = meta.cache_control.as_deref().and_then(parse_max_age);
    server.map_or(config.capabilities_ttl, |value| {
        value.min(config.capabilities_ttl)
    })
}
fn bounded_resolution_ttl(config: &Config, meta: &ResponseMeta) -> Duration {
    let server = meta.cache_control.as_deref().and_then(parse_max_age);
    server.map_or(config.resolution_ttl, |value| {
        value.min(config.resolution_ttl)
    })
}
fn parse_max_age(value: &str) -> Option<Duration> {
    value.split(',').find_map(|part| {
        part.trim()
            .strip_prefix("max-age=")?
            .parse::<u64>()
            .ok()
            .map(Duration::from_secs)
    })
}
fn active_response_body_bytes_max(
    capabilities: &Capabilities,
    response_body_bytes_max: usize,
) -> usize {
    smaller_bound(
        capabilities.bounds.response_body_bytes_max,
        response_body_bytes_max,
    )
}

#[cfg(test)]
mod tests;
