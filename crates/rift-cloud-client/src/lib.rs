//! Bounded client for Rift global package data.

pub mod contract;
mod response;

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
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
use serde::Serialize;
use tokio::{
    sync::{Mutex, RwLock, Semaphore},
    time::{Instant, sleep, timeout_at},
};
use tracing::Instrument as _;

#[expect(
    missing_docs,
    reason = "generated contract source preserves generator output"
)]
#[allow(
    clippy::all,
    clippy::match_same_arms,
    clippy::missing_errors_doc,
    clippy::wildcard_imports,
    unreachable_pub,
    unused_imports,
    reason = "generated wire types stay private behind the bounded runtime"
)]
#[rustfmt::skip]
mod generated;
pub use generated::{
    Capabilities, CapabilityBounds, Documentation, DocumentationFormat, ExactKind, Extensions,
    GetCapabilitiesRequest, GetCapabilitiesResponse, IdentifierMatchClass, Language,
    ListPackageSymbolsRequest, ListPackageSymbolsRequestQuery, ListPackageSymbolsResponse, NodeId,
    PackageAvailability, PackageContextEntry, PackageIdentity, PackageResolutionRequest,
    PackageResolutionResponse, PackageSearchHit, PackageSearchHitContributingField,
    PackageSearchPage, PackageSearchRequest, PackageSearchRequestPhase, PackageSymbol,
    PackageSymbolPage, PackageSymbolRequest, Parameter, ProblemDetails, PublicationFormat,
    QueryTerm, ResolvePackageContextRequest, ResolvePackageContextResponse, ResolvedRequirement,
    SearchPackagesRequest, SearchPackagesRequestQuery, SearchPackagesResponse, Signature,
    SignatureLink, SourceKind, SourceLocationKind, SourceUnitId, Symbol, SymbolFacet, SymbolId,
    SymbolOrigin, TextRange, TypeBinding, TypeBindingOrigin, TypeBindingRole, TypeExpression,
    Warning, WarningCode,
};
pub mod domain;
pub use domain::{PackageSearchCandidate, PackageSymbolCandidate};

/// Most bytes one encoded request body carries.
pub const REQUEST_BODY_BYTES_MAX: usize = 4 * 1024 * 1024;
/// Most bytes one response body carries.
pub const RESPONSE_BODY_BYTES_MAX: usize = 32 * 1024 * 1024;
/// Fewest attempts one request allows.
pub const ATTEMPTS_MIN: u32 = 1;
/// Most attempts one request allows.
pub const ATTEMPTS_MAX: u32 = 5;
/// Most entries one dependency context carries.
pub const DEPENDENCY_ENTRIES_MAX: usize = 20_000;
/// Most UTF-8 bytes one query carries.
pub const QUERY_BYTES_MAX: usize = 4_096;
/// Most terms one query carries.
pub const QUERY_TERMS_MAX: usize = 32;
/// Most UTF-8 bytes one query term carries.
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
pub const PAGE_LIMIT_MAX: i64 = 200;
/// Most warnings one page assembly retains.
pub const WARNINGS_MAX: usize = 32;
/// Most UTF-8 bytes one source payload carries.
pub const SOURCE_BYTES_MAX: usize = 1024 * 1024;
/// Most candidates one page assembly retains.
pub const CANDIDATE_POOL_MAX: usize = 1_000;
/// Most UTF-8 bytes one retained response header carries.
pub const RESPONSE_HEADER_BYTES_MAX: usize = 256;

const DEFAULT_ENDPOINT: &str = "https://api.volar.sh/rift/rest";
const RATE_LIMIT_LIMIT: &str = "ratelimit-limit";
const RATE_LIMIT_REMAINING: &str = "ratelimit-remaining";
const RATE_LIMIT_RESET: &str = "ratelimit-reset";
/// Why global client construction failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// Global access is disabled.
    Disabled,
    /// Endpoint is not an absolute base URL.
    InvalidEndpoint,
    /// Endpoint does not use HTTPS or loopback HTTP.
    InvalidEndpointScheme,
    /// Token environment variable name is invalid.
    InvalidTokenEnvironment,
    /// Token cannot form a bounded authorization header.
    InvalidToken,
    /// Named setting falls outside its accepted range.
    OutOfRange(&'static str),
    /// HTTP client construction failed.
    HttpClient(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("global client disabled"),
            Self::InvalidEndpoint => f.write_str("endpoint must be an absolute base URL without user information, query, fragment, or trailing slash"),
            Self::InvalidEndpointScheme => f.write_str("endpoint must use HTTPS except for loopback HTTP"),
            Self::InvalidTokenEnvironment => f.write_str("token environment variable name is invalid"),
            Self::InvalidToken => f.write_str("token cannot form a bounded authorization header"),
            Self::OutOfRange(name) => write!(f, "{name} is outside configured bounds"),
            Self::HttpClient(message) => write!(f, "building HTTP client failed: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Accepted global client settings.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Config {
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
            enabled: true,
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            token_env: "RIFT_API_TOKEN".to_owned(),
            connect_timeout: Duration::from_secs(3),
            request_timeout: Duration::from_secs(15),
            attempts: 3,
            max_in_flight: 4,
            capabilities_ttl: Duration::from_mins(15),
            resolution_ttl: Duration::from_mins(15),
            failure_ttl: Duration::from_secs(30),
        }
    }
}

impl TryFrom<&rift_protocol::configuration::GlobalConfiguration> for Config {
    type Error = ConfigError;

    fn try_from(
        value: &rift_protocol::configuration::GlobalConfiguration,
    ) -> Result<Self, Self::Error> {
        let attempts =
            u32::try_from(value.attempts).map_err(|_| ConfigError::OutOfRange("attempts"))?;
        let max_in_flight = usize::try_from(value.max_in_flight)
            .map_err(|_| ConfigError::OutOfRange("max_in_flight"))?;
        let config = Self {
            enabled: value.enabled,
            endpoint: value.endpoint.clone(),
            token_env: value.token_env.clone(),
            connect_timeout: Duration::from_millis(value.connect_timeout.milliseconds()),
            request_timeout: Duration::from_millis(value.request_timeout.milliseconds()),
            attempts,
            max_in_flight,
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

/// One global client failure.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientError {
    /// Client settings are invalid.
    Config(ConfigError),
    /// Global access is disabled.
    Disabled,
    /// Credential settings or value are invalid.
    CredentialConfiguration,
    /// Encoded request body crossed its byte bound.
    RequestBodyTooLarge {
        /// Observed bytes.
        bytes: usize,
    },
    /// Response body crossed its byte bound.
    ResponseBodyTooLarge {
        /// Observed bytes.
        bytes: usize,
    },
    /// Total operation deadline elapsed.
    Deadline,
    /// Pending request was cancelled.
    Cancelled,
    /// Endpoint connection or response stream failed.
    Connection,
    /// Successful response carried another media type.
    InvalidMediaType {
        /// HTTP status code.
        status: u16,
        /// Received content type when readable.
        content_type: Option<String>,
    },
    /// Response status or header is not accepted for the operation.
    InvalidResponse {
        /// HTTP status code, or zero before a response exists.
        status: u16,
    },
    /// JSON response decoding failed.
    Decode {
        /// HTTP status code.
        status: u16,
    },
    /// Request field broke a contract bound or relationship.
    InvalidRequest {
        /// Stable field name.
        field: &'static str,
    },
    /// Response field broke a contract bound or relationship.
    InvalidResponseField {
        /// Stable field name.
        field: &'static str,
    },
    /// Endpoint returned a non-success response.
    Http {
        /// Bounded status and headers.
        meta: Box<ResponseMeta>,
        /// Problem Details value when the response carried a valid one.
        problem: Option<Box<ProblemDetails>>,
    },
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => error.fmt(f),
            Self::Disabled => f.write_str("global client disabled"),
            Self::CredentialConfiguration => f.write_str("global credential configuration invalid"),
            Self::RequestBodyTooLarge { bytes } => {
                write!(f, "request body exceeds bound: {bytes} bytes")
            }
            Self::ResponseBodyTooLarge { bytes } => {
                write!(f, "response body exceeds bound: {bytes} bytes")
            }
            Self::Deadline => f.write_str("global request deadline elapsed"),
            Self::Cancelled => f.write_str("global request cancelled"),
            Self::Connection => f.write_str("global endpoint connection failed"),
            Self::InvalidMediaType {
                status,
                content_type,
            } => write!(
                f,
                "invalid response media type for status {status}: {content_type:?}"
            ),
            Self::InvalidResponse { status } => {
                write!(f, "unsupported global response status: {status}")
            }
            Self::Decode { status } => {
                write!(f, "global response decode failed for status {status}")
            }
            Self::InvalidRequest { field } => write!(f, "global request violates bound: {field}"),
            Self::InvalidResponseField { field } => {
                write!(f, "global response violates contract: {field}")
            }
            Self::Http { meta, .. } => write!(f, "global endpoint returned HTTP {}", meta.status),
        }
    }
}

impl std::error::Error for ClientError {}

/// Bounded REST client for global package data.
#[derive(Clone)]
pub struct GlobalClient {
    inner: Arc<Inner>,
}

struct Inner {
    http: HttpClient,
    endpoint: Url,
    enabled: bool,
    config: Config,
    permits: Arc<Semaphore>,
    capabilities: RwLock<Option<CachedCapabilities>>,
    capabilities_flight: Mutex<()>,
    resolutions: RwLock<HashMap<Vec<u8>, CachedResolution>>,
    resolutions_flight: Mutex<()>,
    unavailable_until: RwLock<Option<Instant>>,
}

#[derive(Clone)]
struct CachedCapabilities {
    value: Capabilities,
    etag: Option<String>,
    expires: Instant,
}

#[derive(Clone)]
struct CachedResolution {
    value: PackageResolutionResponse,
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
    pub items: Vec<PackageSearchHit>,
    /// First distinct warnings in page order.
    pub warnings: Vec<Warning>,
    /// Analyzer revision shared by every page.
    pub analyzer_revision: String,
    /// Corpus revision shared by every page.
    pub corpus_revision: String,
}

/// Symbol pages assembled under one capability and candidate bound.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageSymbolPages {
    /// Ordered, duplicate-free declarations.
    pub items: Vec<PackageSymbol>,
    /// First distinct warnings in page order.
    pub warnings: Vec<Warning>,
    /// Analyzer revision shared by every page.
    pub analyzer_revision: String,
    /// Corpus revision shared by every page.
    pub corpus_revision: String,
}

impl GlobalClient {
    /// Builds one client and captures the configured credential value.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when enabled settings or credentials are invalid.
    pub fn new(config: Config) -> Result<Self, ConfigError> {
        let token = if config.enabled {
            read_token(&config.token_env)?
        } else {
            None
        };
        Self::new_with_token(config, token)
    }

    fn new_with_token(config: Config, token: Option<String>) -> Result<Self, ConfigError> {
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
            let header =
                HeaderValue::from_str(&authorization).map_err(|_| ConfigError::InvalidToken)?;
            headers.insert(AUTHORIZATION, header);
            builder = builder.default_headers(headers);
        }
        let http = builder
            .build()
            .map_err(|error| ConfigError::HttpClient(error.to_string()))?;
        let enabled = config.enabled;
        let max_in_flight = config.max_in_flight;
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                endpoint,
                enabled,
                permits: Arc::new(Semaphore::new(max_in_flight)),
                capabilities: RwLock::new(None),
                capabilities_flight: Mutex::new(()),
                resolutions: RwLock::new(HashMap::new()),
                resolutions_flight: Mutex::new(()),
                unavailable_until: RwLock::new(None),
                config,
            }),
        })
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
    /// Returns [`ClientError`] when access is disabled or no valid answer is available.
    pub async fn get_capabilities(&self) -> Result<Capabilities, ClientError> {
        if !self.inner.enabled {
            return Err(ClientError::Disabled);
        }
        if self.is_unavailable().await {
            return Err(ClientError::Connection);
        }
        let _flight = self.inner.capabilities_flight.lock().await;
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
                RESPONSE_BODY_BYTES_MAX,
            )
            .await;
        let result = match response {
            Ok(raw) => match response::capabilities(raw).await {
                Ok(response::ParsedCapabilities::NotModified(meta)) => {
                    let mut cache = self.inner.capabilities.write().await;
                    let Some(cached) = cache.as_mut() else {
                        return Err(ClientError::InvalidResponse { status: 304 });
                    };
                    cached.expires =
                        Instant::now() + bounded_capabilities_ttl(&self.inner.config, &meta);
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
                    *self.inner.capabilities.write().await = Some(cached);
                    *self.inner.unavailable_until.write().await = None;
                    Ok(value)
                }
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        if result.is_err() {
            self.record_failure().await;
        }
        result
    }

    /// Resolves one canonical dependency context into exact package identities.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the request or answer breaks the contract, or the endpoint is
    /// unavailable.
    pub async fn resolve_package_context(
        &self,
        request: &PackageResolutionRequest,
    ) -> Result<PackageResolutionResponse, ClientError> {
        if !self.inner.enabled {
            return Err(ClientError::Disabled);
        }
        validate_resolution_request(request)?;
        let capabilities = self.get_capabilities().await?;
        validate_resolution_request_for_capabilities(request, &capabilities)?;
        let body = serialize_body(request)?;
        validate_body_for_capabilities(&body, &capabilities)?;
        let key = resolution_cache_key(&capabilities, &body)?;
        if let Some(value) = self.inner.resolutions.read().await.get(&key)
            && value.expires > Instant::now()
        {
            return Ok(value.value.clone());
        }
        let _flight = self.inner.resolutions_flight.lock().await;
        if let Some(value) = self.inner.resolutions.read().await.get(&key)
            && value.expires > Instant::now()
        {
            return Ok(value.value.clone());
        }
        let raw = match self
            .request(
                contract::Endpoint::Resolutions,
                Some(body),
                None,
                active_response_body_bytes_max(&capabilities),
            )
            .await
        {
            Ok(raw) => raw,
            Err(error) => {
                self.record_failure().await;
                return Err(error);
            }
        };
        let response::Parsed { value, meta } = match response::resolution(raw).await {
            Ok(parsed) => parsed,
            Err(error) => {
                self.record_failure().await;
                return Err(error);
            }
        };
        if let Err(error) = validate_resolution_response(request, &value) {
            self.record_failure().await;
            return Err(error);
        }
        let ttl = bounded_resolution_ttl(&self.inner.config, &meta);
        self.inner.resolutions.write().await.insert(
            key,
            CachedResolution {
                value: value.clone(),
                expires: Instant::now() + ttl,
            },
        );
        Ok(value)
    }

    /// Reads one package search page.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the request or answer breaks the contract, or the endpoint is
    /// unavailable.
    pub async fn search_packages(
        &self,
        request: &PackageSearchRequest,
        limit: i64,
        cursor: Option<&str>,
    ) -> Result<PackageSearchPage, ClientError> {
        if !self.inner.enabled {
            return Err(ClientError::Disabled);
        }
        validate_search_request(request)?;
        validate_page(limit, cursor)?;
        let capabilities = self.get_capabilities().await?;
        validate_search_request_for_capabilities(request, limit, cursor, &capabilities)?;
        let body = serialize_body(request)?;
        validate_body_for_capabilities(&body, &capabilities)?;
        let query = SearchPackagesRequestQuery {
            limit: Some(limit),
            cursor: cursor.map(str::to_owned),
        };
        let raw = match self
            .request_with_query(
                contract::Endpoint::Search,
                body,
                &query,
                active_response_body_bytes_max(&capabilities),
            )
            .await
        {
            Ok(raw) => raw,
            Err(error) => {
                self.record_failure().await;
                return Err(error);
            }
        };
        let response::Parsed { value: page, .. } = match response::search(raw).await {
            Ok(parsed) => parsed,
            Err(error) => {
                self.record_failure().await;
                return Err(error);
            }
        };
        if let Err(error) = validate_search_page(request, &capabilities, &page, cursor) {
            self.record_failure().await;
            return Err(error);
        }
        Ok(page)
    }

    /// Reads one package symbol page.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when the request or answer breaks the contract, or the endpoint is
    /// unavailable.
    pub async fn list_package_symbols(
        &self,
        request: &PackageSymbolRequest,
        limit: i64,
        cursor: Option<&str>,
    ) -> Result<PackageSymbolPage, ClientError> {
        if !self.inner.enabled {
            return Err(ClientError::Disabled);
        }
        validate_symbol_request(request)?;
        validate_page(limit, cursor)?;
        let capabilities = self.get_capabilities().await?;
        validate_symbol_request_for_capabilities(request, limit, cursor, &capabilities)?;
        let body = serialize_body(request)?;
        validate_body_for_capabilities(&body, &capabilities)?;
        let query = ListPackageSymbolsRequestQuery {
            limit: Some(limit),
            cursor: cursor.map(str::to_owned),
        };
        let raw = match self
            .request_with_query(
                contract::Endpoint::Symbols,
                body,
                &query,
                active_response_body_bytes_max(&capabilities),
            )
            .await
        {
            Ok(raw) => raw,
            Err(error) => {
                self.record_failure().await;
                return Err(error);
            }
        };
        let response::Parsed { value: page, .. } = match response::symbols(raw).await {
            Ok(parsed) => parsed,
            Err(error) => {
                self.record_failure().await;
                return Err(error);
            }
        };
        if let Err(error) = validate_symbol_page(request, &capabilities, &page, cursor) {
            self.record_failure().await;
            return Err(error);
        }
        Ok(page)
    }

    /// Reads package search pages through the active candidate bound.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when any page is unavailable, changes revision, repeats an identity,
    /// or repeats a cursor.
    pub async fn search_packages_pages(
        &self,
        request: &PackageSearchRequest,
        limit: i64,
    ) -> Result<PackageSearchPages, ClientError> {
        validate_search_request(request)?;
        validate_page(limit, None)?;
        let capabilities = self.get_capabilities().await?;
        validate_search_request_for_capabilities(request, limit, None, &capabilities)?;
        let candidate_max = bounded_candidate_pool(&capabilities);
        let mut cursor = None;
        let mut hits = Vec::new();
        let mut warnings = Vec::new();
        let mut revisions = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let page = self
                .search_packages(request, limit, cursor.as_deref())
                .await?;
            if let Err(error) = validate_assembled_revision(
                &mut revisions,
                &page.analyzer_revision,
                &page.corpus_revision,
            ) {
                self.record_failure().await;
                return Err(error);
            }
            extend_warnings(&mut warnings, page.warnings);
            for hit in page.items {
                let identity = search_hit_identity(&hit);
                if hits
                    .iter()
                    .any(|item: &PackageSearchHit| search_hit_identity(item) == identity)
                {
                    self.record_failure().await;
                    return Err(ClientError::InvalidResponseField {
                        field: "duplicate_item",
                    });
                }
                hits.push(hit);
                if hits.len() >= candidate_max {
                    return Ok(search_pages(hits, warnings, revisions));
                }
            }
            let Some(next) = page.next_cursor else {
                return Ok(search_pages(hits, warnings, revisions));
            };
            if !seen_cursors.insert(next.clone()) {
                self.record_failure().await;
                return Err(ClientError::InvalidResponseField {
                    field: "cursor_progress",
                });
            }
            cursor = Some(next);
        }
    }

    /// Reads package symbol pages through the active candidate bound.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError`] when any page is unavailable, changes revision, repeats an identity,
    /// or repeats a cursor.
    pub async fn list_package_symbols_pages(
        &self,
        request: &PackageSymbolRequest,
        limit: i64,
    ) -> Result<PackageSymbolPages, ClientError> {
        validate_symbol_request(request)?;
        validate_page(limit, None)?;
        let capabilities = self.get_capabilities().await?;
        validate_symbol_request_for_capabilities(request, limit, None, &capabilities)?;
        let candidate_max = bounded_candidate_pool(&capabilities);
        let mut cursor = None;
        let mut hits = Vec::new();
        let mut warnings = Vec::new();
        let mut revisions = None;
        let mut seen_cursors = HashSet::new();
        loop {
            let page = self
                .list_package_symbols(request, limit, cursor.as_deref())
                .await?;
            if let Err(error) = validate_assembled_revision(
                &mut revisions,
                &page.analyzer_revision,
                &page.corpus_revision,
            ) {
                self.record_failure().await;
                return Err(error);
            }
            extend_warnings(&mut warnings, page.warnings);
            for hit in page.items {
                let identity = symbol_hit_identity(&hit);
                if hits
                    .iter()
                    .any(|item: &PackageSymbol| symbol_hit_identity(item) == identity)
                {
                    self.record_failure().await;
                    return Err(ClientError::InvalidResponseField {
                        field: "duplicate_item",
                    });
                }
                hits.push(hit);
                if hits.len() >= candidate_max {
                    return Ok(symbol_pages(hits, warnings, revisions));
                }
            }
            let Some(next) = page.next_cursor else {
                return Ok(symbol_pages(hits, warnings, revisions));
            };
            if !seen_cursors.insert(next.clone()) {
                self.record_failure().await;
                return Err(ClientError::InvalidResponseField {
                    field: "cursor_progress",
                });
            }
            cursor = Some(next);
        }
    }

    async fn request_with_query<Q: Serialize + Sync + ?Sized>(
        &self,
        operation: contract::Endpoint,
        body: Vec<u8>,
        query: &Q,
        response_body_bytes_max: usize,
    ) -> Result<RawResponse, ClientError> {
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
    ) -> Result<RawResponse, ClientError> {
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
    ) -> Result<RawResponse, ClientError> {
        let started = Instant::now();
        let deadline = Instant::now() + self.inner.config.request_timeout;
        let attempts = self.inner.config.attempts;
        let span = tracing::info_span!(
            "global.request",
            component = "global",
            operation = operation.operation_id(),
            request_count = tracing::field::Empty,
            latency_ms = tracing::field::Empty,
            response_bytes = tracing::field::Empty,
            status = tracing::field::Empty,
            retry_count = tracing::field::Empty,
        );
        let record = span.clone();
        async move {
            let mut attempt = 0;
            loop {
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
                        if error == ClientError::Connection
                            && attempt < attempts
                            && Instant::now() < deadline
                        {
                            continue;
                        }
                        record_request_span(&record, started, attempt, None);
                        return Err(error);
                    }
                    Err(_) => {
                        record_request_span(&record, started, attempt, None);
                        return Err(ClientError::Deadline);
                    }
                };
                tracing::debug!(
                    operation = operation.operation_id(),
                    attempt,
                    retry_count = attempt.saturating_sub(1),
                    status = response.status.as_u16(),
                    response_bytes = response.body.len(),
                    elapsed_ms = started.elapsed().as_millis(),
                    "global response"
                );
                if should_retry(response.status, attempt, attempts) {
                    tracing::debug!(
                        operation = operation.operation_id(),
                        attempt,
                        status = response.status.as_u16(),
                        "retrying global request"
                    );
                    if let Some(delay) = retry_delay(&response.meta) {
                        if delay >= deadline.saturating_duration_since(Instant::now()) {
                            record_request_span(&record, started, attempt, Some(&response));
                            return Ok(response);
                        }
                        if timeout_at(deadline, sleep(delay)).await.is_err() {
                            record_request_span(&record, started, attempt, None);
                            return Err(ClientError::Deadline);
                        }
                    }
                    continue;
                }
                record_request_span(&record, started, attempt, Some(&response));
                return Ok(response);
            }
        }
        .instrument(span)
        .await
    }

    async fn send_once<Q: Serialize + Sync + ?Sized>(
        &self,
        operation: contract::Endpoint,
        body: Option<&[u8]>,
        query: Option<&Q>,
        etag: Option<&str>,
        response_body_bytes_max: usize,
    ) -> Result<RawResponse, ClientError> {
        let _permit = self
            .inner
            .permits
            .acquire()
            .await
            .map_err(|_| ClientError::Cancelled)?;
        let url = make_url(&self.inner.endpoint, operation.path());
        let method = operation.method();
        let mut request = self
            .inner
            .http
            .request(method, url)
            .header(ACCEPT, "application/json, application/problem+json");
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
                ClientError::Deadline
            } else {
                ClientError::Connection
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
        if response
            .content_length()
            .is_some_and(|size| size > response_body_bytes_max as u64)
        {
            let bytes = match response.content_length() {
                Some(size) => match usize::try_from(size) {
                    Ok(value) => value,
                    Err(_) => usize::MAX,
                },
                None => response_body_bytes_max.saturating_add(1),
            };
            return Err(ClientError::ResponseBodyTooLarge { bytes });
        }
        let mut body_bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ClientError::Connection)?
        {
            let next = body_bytes.len().saturating_add(chunk.len());
            if next > response_body_bytes_max {
                return Err(ClientError::ResponseBodyTooLarge { bytes: next });
            }
            body_bytes.extend_from_slice(&chunk);
        }
        if status.is_success()
            && status != StatusCode::NO_CONTENT
            && !is_media(&meta, "application/json")
        {
            return Err(ClientError::InvalidMediaType {
                status: status.as_u16(),
                content_type: meta.content_type,
            });
        }
        Ok(RawResponse {
            status,
            body: body_bytes,
            meta,
        })
    }

    async fn is_unavailable(&self) -> bool {
        self.inner
            .unavailable_until
            .read()
            .await
            .is_some_and(|until| until > Instant::now())
    }
    async fn record_failure(&self) {
        *self.inner.unavailable_until.write().await =
            Some(Instant::now() + self.inner.config.failure_ttl);
    }
}

fn record_request_span(
    span: &tracing::Span,
    started: Instant,
    request_count: u32,
    response: Option<&RawResponse>,
) {
    span.record("request_count", request_count);
    span.record("retry_count", request_count.saturating_sub(1));
    span.record("latency_ms", started.elapsed().as_millis());
    if let Some(response) = response {
        span.record("status", response.status.as_u16());
        span.record("response_bytes", response.body.len());
    }
}

impl ResponseMeta {
    fn from_headers(status: StatusCode, headers: &HeaderMap) -> Result<Self, ClientError> {
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

fn validate_capabilities(value: &Capabilities) -> Result<(), ClientError> {
    if value.publication_format != PublicationFormat::RiftPackageIndexV1 {
        return Err(ClientError::InvalidResponseField {
            field: "publication_format",
        });
    }
    if value.supported_package_managers.is_empty()
        || value.supported_package_managers.len() > 3
        || value
            .supported_package_managers
            .iter()
            .any(|manager| manager.is_empty() || manager.len() > 128)
    {
        return Err(ClientError::InvalidResponseField {
            field: "supported_package_managers",
        });
    }
    let required_features = ["resolutions", "search", "symbols"];
    if value.supported_features.is_empty()
        || value.supported_features.len() > 32
        || value
            .supported_features
            .iter()
            .any(|feature| feature.is_empty() || feature.len() > 128)
        || required_features.iter().any(|required| {
            !value
                .supported_features
                .iter()
                .any(|feature| feature == required)
        })
    {
        return Err(ClientError::InvalidResponseField {
            field: "supported_features",
        });
    }
    if value.analyzer_revision.is_empty()
        || value.analyzer_revision.len() > 128
        || value.corpus_revision.is_empty()
        || value.corpus_revision.len() > 128
    {
        return Err(ClientError::InvalidResponseField { field: "revision" });
    }
    let bounds = &value.bounds;
    let bounds_ok = positive_within(bounds.request_body_bytes_max, REQUEST_BODY_BYTES_MAX)
        && positive_within(bounds.response_body_bytes_max, RESPONSE_BODY_BYTES_MAX)
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
        && positive_within(bounds.source_bytes_max, SOURCE_BYTES_MAX)
        && bounds.page_limit_min <= bounds.page_limit_max
        && bounds.page_limit_default >= bounds.page_limit_min
        && bounds.page_limit_default <= bounds.page_limit_max;
    if !bounds_ok {
        return Err(ClientError::InvalidResponseField { field: "bounds" });
    }
    let known_fields = [
        "name",
        "qualified_name",
        "documentation",
        "signature",
        "declaration_source",
    ];
    if value.required_search_fields.len() != known_fields.len()
        || known_fields.iter().any(|required| {
            !value
                .required_search_fields
                .iter()
                .any(|field| field == required)
        })
    {
        return Err(ClientError::InvalidResponseField {
            field: "required_search_fields",
        });
    }
    Ok(())
}

fn validate_resolution_request_for_capabilities(
    request: &PackageResolutionRequest,
    capabilities: &Capabilities,
) -> Result<(), ClientError> {
    if request.entries.len()
        > smaller_bound(
            capabilities.bounds.dependency_entries_max,
            DEPENDENCY_ENTRIES_MAX,
        )
    {
        return Err(ClientError::InvalidRequest { field: "entries" });
    }
    Ok(())
}

fn validate_search_request_for_capabilities(
    request: &PackageSearchRequest,
    limit: i64,
    cursor: Option<&str>,
    capabilities: &Capabilities,
) -> Result<(), ClientError> {
    let bounds = &capabilities.bounds;
    if request.query.len() > smaller_bound(bounds.query_bytes_max, QUERY_BYTES_MAX) {
        return Err(ClientError::InvalidRequest { field: "query" });
    }
    if request.terms.len() > smaller_bound(bounds.query_terms_max, QUERY_TERMS_MAX)
        || request.terms.iter().any(|term| {
            term.text.len() > smaller_bound(bounds.query_term_bytes_max, QUERY_TERM_BYTES_MAX)
        })
    {
        return Err(ClientError::InvalidRequest { field: "terms" });
    }
    if request.identifiers.len() > smaller_bound(bounds.identifiers_max, IDENTIFIERS_MAX)
        || request.identifiers.iter().any(|identifier| {
            identifier.len() > smaller_bound(bounds.identifier_bytes_max, IDENTIFIER_BYTES_MAX)
        })
    {
        return Err(ClientError::InvalidRequest {
            field: "identifiers",
        });
    }
    validate_read_bounds(request.packages.len(), limit, cursor, capabilities)
}

fn validate_symbol_request_for_capabilities(
    request: &PackageSymbolRequest,
    limit: i64,
    cursor: Option<&str>,
    capabilities: &Capabilities,
) -> Result<(), ClientError> {
    if request.name.len() > smaller_bound(capabilities.bounds.query_bytes_max, QUERY_BYTES_MAX) {
        return Err(ClientError::InvalidRequest { field: "name" });
    }
    validate_read_bounds(request.packages.len(), limit, cursor, capabilities)
}

fn validate_read_bounds(
    packages: usize,
    limit: i64,
    cursor: Option<&str>,
    capabilities: &Capabilities,
) -> Result<(), ClientError> {
    let bounds = &capabilities.bounds;
    if packages > smaller_bound(bounds.packages_max, PACKAGES_MAX) {
        return Err(ClientError::InvalidRequest { field: "packages" });
    }
    if limit < bounds.page_limit_min || limit > bounds.page_limit_max {
        return Err(ClientError::InvalidRequest { field: "limit" });
    }
    if cursor
        .is_some_and(|value| value.len() > smaller_bound(bounds.cursor_bytes_max, CURSOR_BYTES_MAX))
    {
        return Err(ClientError::InvalidRequest { field: "cursor" });
    }
    Ok(())
}

fn validate_resolution_request(request: &PackageResolutionRequest) -> Result<(), ClientError> {
    if request.entries.len() > DEPENDENCY_ENTRIES_MAX {
        return Err(ClientError::InvalidRequest { field: "entries" });
    }
    let mut seen = HashSet::new();
    for entry in &request.entries {
        if entry.availability != PackageAvailability::Canonical {
            return Err(ClientError::InvalidRequest {
                field: "availability",
            });
        }
        bounded_nonempty(&entry.manager, 128, "manager")?;
        bounded_nonempty(&entry.name, IDENTIFIER_BYTES_MAX, "name")?;
        if entry.version.is_some() == entry.requirement.is_some() {
            return Err(ClientError::InvalidRequest {
                field: "version_or_requirement",
            });
        }
        if entry
            .version
            .as_ref()
            .is_some_and(|value| value.len() > IDENTIFIER_BYTES_MAX)
            || entry
                .requirement
                .as_ref()
                .is_some_and(|value| value.len() > IDENTIFIER_BYTES_MAX)
        {
            return Err(ClientError::InvalidRequest { field: "selector" });
        }
        let key = context_key(entry);
        if !seen.insert(key) {
            return Err(ClientError::InvalidRequest {
                field: "duplicate_entry",
            });
        }
    }
    Ok(())
}

fn validate_resolution_response(
    request: &PackageResolutionRequest,
    response: &PackageResolutionResponse,
) -> Result<(), ClientError> {
    let expected: HashSet<_> = request.entries.iter().map(context_key).collect();
    let mut seen = HashSet::new();
    for package in &response.available_exact {
        let key = (
            package.manager.clone(),
            package.name.clone(),
            Some(package.version.clone()),
            None,
        );
        if !expected.contains(&key) || !seen.insert(key) {
            return Err(ClientError::InvalidResponseField {
                field: "resolution_accounting",
            });
        }
    }
    for package in &response.missing_exact {
        let key = (
            package.manager.clone(),
            package.name.clone(),
            Some(package.version.clone()),
            None,
        );
        if !expected.contains(&key) || !seen.insert(key) {
            return Err(ClientError::InvalidResponseField {
                field: "resolution_accounting",
            });
        }
    }
    for resolved in &response.resolved_requirements {
        if resolved.package.manager != resolved.entry.manager
            || resolved.package.name != resolved.entry.name
            || resolved.entry.requirement.is_none()
            || resolved.entry.version.is_some()
        {
            return Err(ClientError::InvalidResponseField {
                field: "resolved_requirement",
            });
        }
        let key = context_key(&resolved.entry);
        if !expected.contains(&key) || !seen.insert(key) {
            return Err(ClientError::InvalidResponseField {
                field: "resolution_accounting",
            });
        }
    }
    for entry in &response.missing_requirements {
        if entry.requirement.is_none()
            || entry.version.is_some()
            || entry.availability != PackageAvailability::Canonical
        {
            return Err(ClientError::InvalidResponseField {
                field: "missing_requirement",
            });
        }
        let key = context_key(entry);
        if !expected.contains(&key) || !seen.insert(key) {
            return Err(ClientError::InvalidResponseField {
                field: "resolution_accounting",
            });
        }
    }
    if seen.len() != expected.len() {
        return Err(ClientError::InvalidResponseField {
            field: "resolution_accounting",
        });
    }
    Ok(())
}

fn validate_search_request(request: &PackageSearchRequest) -> Result<(), ClientError> {
    bounded_nonempty(&request.query, QUERY_BYTES_MAX, "query")?;
    if request.terms.is_empty() || request.terms.len() > QUERY_TERMS_MAX {
        return Err(ClientError::InvalidRequest { field: "terms" });
    }
    for term in &request.terms {
        bounded_nonempty(&term.text, QUERY_TERM_BYTES_MAX, "term")?;
        if term.phrase && term.prefix {
            return Err(ClientError::InvalidRequest {
                field: "phrase_prefix",
            });
        }
        if term.prefix
            && term
                .text
                .chars()
                .filter(|character| character.is_alphanumeric())
                .count()
                < 3
        {
            return Err(ClientError::InvalidRequest { field: "prefix" });
        }
    }
    if request.identifiers.len() > IDENTIFIERS_MAX
        || request
            .identifiers
            .iter()
            .any(|value| value.is_empty() || value.len() > IDENTIFIER_BYTES_MAX)
    {
        return Err(ClientError::InvalidRequest {
            field: "identifiers",
        });
    }
    if request
        .include
        .as_ref()
        .is_some_and(|values| values.len() > 1 || values.iter().any(|value| value != "source"))
    {
        return Err(ClientError::InvalidRequest { field: "include" });
    }
    validate_packages(&request.packages)?;
    if matches!(request.phase, PackageSearchRequestPhase::Broad)
        && request.terms.iter().filter(|term| !term.phrase).count() < 2
    {
        return Err(ClientError::InvalidRequest {
            field: "broad_phase",
        });
    }
    Ok(())
}

fn validate_symbol_request(request: &PackageSymbolRequest) -> Result<(), ClientError> {
    bounded_nonempty(&request.name, QUERY_BYTES_MAX, "name")?;
    if request
        .include
        .as_ref()
        .is_some_and(|values| values.len() > 1 || values.iter().any(|value| value != "source"))
    {
        return Err(ClientError::InvalidRequest { field: "include" });
    }
    validate_packages(&request.packages)
}

fn validate_packages(packages: &[PackageIdentity]) -> Result<(), ClientError> {
    if packages.len() > PACKAGES_MAX {
        return Err(ClientError::InvalidRequest { field: "packages" });
    }
    let mut seen = HashSet::new();
    for package in packages {
        bounded_nonempty(&package.manager, 128, "package_manager")?;
        bounded_nonempty(&package.name, IDENTIFIER_BYTES_MAX, "package_name")?;
        bounded_nonempty(&package.version, IDENTIFIER_BYTES_MAX, "package_version")?;
        if !seen.insert((
            package.manager.as_str(),
            package.name.as_str(),
            package.version.as_str(),
        )) {
            return Err(ClientError::InvalidRequest {
                field: "duplicate_package",
            });
        }
    }
    Ok(())
}

fn validate_page(limit: i64, cursor: Option<&str>) -> Result<(), ClientError> {
    if !(PAGE_LIMIT_MIN..=PAGE_LIMIT_MAX).contains(&limit) {
        return Err(ClientError::InvalidRequest { field: "limit" });
    }
    if cursor.is_some_and(|value| value.is_empty() || value.len() > CURSOR_BYTES_MAX) {
        return Err(ClientError::InvalidRequest { field: "cursor" });
    }
    Ok(())
}

fn validate_search_page(
    request: &PackageSearchRequest,
    capabilities: &Capabilities,
    page: &PackageSearchPage,
    cursor: Option<&str>,
) -> Result<(), ClientError> {
    validate_page_metadata(
        capabilities,
        &page.publication_format,
        &page.analyzer_revision,
        &page.corpus_revision,
        page.next_cursor.as_deref(),
        &page.warnings,
    )?;
    let packages: HashSet<_> = request.packages.iter().map(package_key).collect();
    let mut seen = HashSet::new();
    for hit in &page.items {
        if hit.source.is_some() && !includes_source(request.include.as_deref()) {
            return Err(ClientError::InvalidResponseField { field: "source" });
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
            &packages,
            smaller_bound(capabilities.bounds.source_bytes_max, SOURCE_BYTES_MAX),
        )?;
        validate_search_match_class(request, hit, &qualified_name)?;
        let key = search_hit_identity(hit);
        if !seen.insert(key) {
            return Err(ClientError::InvalidResponseField {
                field: "duplicate_item",
            });
        }
    }
    if cursor.is_some() && page.items.is_empty() && page.next_cursor.as_deref() == cursor {
        return Err(ClientError::InvalidResponseField {
            field: "cursor_progress",
        });
    }
    Ok(())
}

fn validate_symbol_page(
    request: &PackageSymbolRequest,
    capabilities: &Capabilities,
    page: &PackageSymbolPage,
    cursor: Option<&str>,
) -> Result<(), ClientError> {
    validate_page_metadata(
        capabilities,
        &page.publication_format,
        &page.analyzer_revision,
        &page.corpus_revision,
        page.next_cursor.as_deref(),
        &page.warnings,
    )?;
    let packages: HashSet<_> = request.packages.iter().map(package_key).collect();
    let mut seen = HashSet::new();
    for hit in &page.items {
        if hit.source.is_some() && !includes_source(request.include.as_deref()) {
            return Err(ClientError::InvalidResponseField { field: "source" });
        }
        if request
            .language
            .as_ref()
            .is_some_and(|language| language != &hit.symbol.language)
        {
            return Err(ClientError::InvalidResponseField { field: "language" });
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
            &packages,
            smaller_bound(capabilities.bounds.source_bytes_max, SOURCE_BYTES_MAX),
        )?;
        validate_symbol_match_class(request, hit, &qualified_name)?;
        if !seen.insert(symbol_hit_identity(hit)) {
            return Err(ClientError::InvalidResponseField {
                field: "duplicate_item",
            });
        }
    }
    if cursor.is_some() && page.items.is_empty() && page.next_cursor.as_deref() == cursor {
        return Err(ClientError::InvalidResponseField {
            field: "cursor_progress",
        });
    }
    Ok(())
}

fn validate_page_metadata(
    capabilities: &Capabilities,
    publication_format: &PublicationFormat,
    analyzer_revision: &str,
    corpus_revision: &str,
    next_cursor: Option<&str>,
    warnings: &[Warning],
) -> Result<(), ClientError> {
    if publication_format != &capabilities.publication_format {
        return Err(ClientError::InvalidResponseField {
            field: "publication_format",
        });
    }
    if analyzer_revision.is_empty()
        || analyzer_revision.len() > 128
        || corpus_revision != capabilities.corpus_revision
    {
        return Err(ClientError::InvalidResponseField { field: "revision" });
    }
    if next_cursor.is_some_and(|value| {
        value.is_empty()
            || value.len() > smaller_bound(capabilities.bounds.cursor_bytes_max, CURSOR_BYTES_MAX)
    }) {
        return Err(ClientError::InvalidResponseField { field: "cursor" });
    }
    if warnings.len() > smaller_bound(capabilities.bounds.warnings_max, WARNINGS_MAX)
        || warnings.iter().any(|warning| {
            warning
                .detail
                .as_ref()
                .is_some_and(|detail| detail.is_empty() || detail.len() > 1024)
        })
    {
        return Err(ClientError::InvalidResponseField { field: "warnings" });
    }
    Ok(())
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
    packages: &HashSet<(String, String, String)>,
    source_bytes_max: usize,
) -> Result<String, ClientError> {
    if !packages.contains(&package_key(package)) {
        return Err(ClientError::InvalidResponseField { field: "package" });
    }
    if location.line < 1 || location.range.end < location.range.start {
        return Err(ClientError::InvalidResponseField { field: "location" });
    }
    if location
        .source
        .is_some_and(|value| value.len() > source_bytes_max)
    {
        return Err(ClientError::InvalidResponseField { field: "source" });
    }
    let unit = rift_core::SourceUnitId::parse(location.unit).map_err(|_| {
        ClientError::InvalidResponseField {
            field: "source_identity",
        }
    })?;
    let package_prefix = format!("{}@{}/", package.name, package.version);
    let source_path = unit
        .key()
        .as_str()
        .strip_prefix(&package_prefix)
        .filter(|path| !path.is_empty())
        .ok_or(ClientError::InvalidResponseField {
            field: "source_identity",
        })?;
    if unit.resolver().as_str() != package.manager {
        return Err(ClientError::InvalidResponseField {
            field: "source_identity",
        });
    }
    let Some(origin) = symbol.origin.as_ref() else {
        return Err(ClientError::InvalidResponseField { field: "origin" });
    };
    if origin.location != Some(SourceLocationKind::Dependency)
        || origin.package.as_ref() != Some(package)
    {
        return Err(ClientError::InvalidResponseField { field: "origin" });
    }
    validate_symbol_identity(symbol, source_path)
}

fn validate_symbol_identity(symbol: &Symbol, source_path: &str) -> Result<String, ClientError> {
    let id = symbol
        .id
        .as_deref()
        .ok_or(ClientError::InvalidResponseField {
            field: "symbol_identity",
        })?;
    if id.len() > 8_192 || rift_core::SymbolId::new(id).is_err() {
        return Err(ClientError::InvalidResponseField {
            field: "symbol_identity",
        });
    }
    let address = id
        .strip_prefix("rift://symbol/")
        .ok_or(ClientError::InvalidResponseField {
            field: "symbol_identity",
        })?;
    let (language, remainder) =
        address
            .split_once('/')
            .ok_or(ClientError::InvalidResponseField {
                field: "symbol_identity",
            })?;
    if language != symbol.language
        || rift_protocol::read::Language::from_identity_segment(language).is_err()
    {
        return Err(ClientError::InvalidResponseField {
            field: "symbol_identity",
        });
    }
    let (_, encoded_qualified_name) =
        remainder
            .rsplit_once('/')
            .ok_or(ClientError::InvalidResponseField {
                field: "symbol_identity",
            })?;
    let qualified_name = percent_decode_str(encoded_qualified_name)
        .decode_utf8()
        .map_err(|_| ClientError::InvalidResponseField {
            field: "symbol_identity",
        })?;
    if qualified_name.is_empty()
        || rift_core::symbol_identity(language, source_path, &qualified_name) != id
    {
        return Err(ClientError::InvalidResponseField {
            field: "symbol_identity",
        });
    }
    Ok(qualified_name.into_owned())
}

fn validate_search_match_class(
    request: &PackageSearchRequest,
    hit: &PackageSearchHit,
    qualified_name: &str,
) -> Result<(), ClientError> {
    let actual = ranking_match_class(&hit.match_class)?;
    if request.identifiers.is_empty() {
        return Ok(());
    }
    let name = hit.symbol.name.to_lowercase();
    let qualified_name = qualified_name.to_lowercase();
    let expected = request
        .identifiers
        .iter()
        .filter_map(|candidate| {
            rift_ranking::match_class(&candidate.to_lowercase(), &name, &qualified_name)
        })
        .min();
    if expected != Some(actual) {
        return Err(ClientError::InvalidResponseField {
            field: "match_class",
        });
    }
    Ok(())
}

fn validate_symbol_match_class(
    request: &PackageSymbolRequest,
    hit: &PackageSymbol,
    qualified_name: &str,
) -> Result<(), ClientError> {
    let actual = ranking_match_class(&hit.match_class)?;
    let expected = rift_ranking::match_class(
        &request.name.to_lowercase(),
        &hit.symbol.name.to_lowercase(),
        &qualified_name.to_lowercase(),
    );
    if expected != Some(actual) {
        return Err(ClientError::InvalidResponseField {
            field: "match_class",
        });
    }
    Ok(())
}

fn ranking_match_class(
    value: &IdentifierMatchClass,
) -> Result<rift_ranking::IdentifierMatchClass, ClientError> {
    match value {
        IdentifierMatchClass::QualifiedExact => {
            Ok(rift_ranking::IdentifierMatchClass::QualifiedExact)
        }
        IdentifierMatchClass::NameExact => Ok(rift_ranking::IdentifierMatchClass::NameExact),
        IdentifierMatchClass::NamePrefix => Ok(rift_ranking::IdentifierMatchClass::NamePrefix),
        IdentifierMatchClass::Substring => Ok(rift_ranking::IdentifierMatchClass::Substring),
        IdentifierMatchClass::Unknown => Err(ClientError::InvalidResponseField {
            field: "match_class",
        }),
    }
}

fn bounded_nonempty(value: &str, max: usize, field: &'static str) -> Result<(), ClientError> {
    if value.is_empty() || value.len() > max {
        return Err(ClientError::InvalidRequest { field });
    }
    Ok(())
}

fn package_key(package: &PackageIdentity) -> (String, String, String) {
    (
        package.manager.clone(),
        package.name.clone(),
        package.version.clone(),
    )
}

fn context_key(entry: &PackageContextEntry) -> (String, String, Option<String>, Option<String>) {
    (
        entry.manager.clone(),
        entry.name.clone(),
        entry.version.clone(),
        entry.requirement.clone(),
    )
}

fn resolution_cache_key(capabilities: &Capabilities, body: &[u8]) -> Result<Vec<u8>, ClientError> {
    serde_json::to_vec(&(
        capabilities.analyzer_revision.as_str(),
        capabilities.corpus_revision.as_str(),
        body,
    ))
    .map_err(|_| ClientError::Decode { status: 0 })
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
    revisions: &mut Option<(String, String)>,
    analyzer_revision: &str,
    corpus_revision: &str,
) -> Result<(), ClientError> {
    match revisions {
        Some((analyzer, corpus)) if analyzer != analyzer_revision || corpus != corpus_revision => {
            Err(ClientError::InvalidResponseField { field: "revision" })
        }
        Some(_) => Ok(()),
        None => {
            *revisions = Some((analyzer_revision.to_owned(), corpus_revision.to_owned()));
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

fn assembled_revisions(revisions: Option<(String, String)>) -> (String, String) {
    revisions.unwrap_or_default()
}

fn search_pages(
    items: Vec<PackageSearchHit>,
    warnings: Vec<Warning>,
    revisions: Option<(String, String)>,
) -> PackageSearchPages {
    let (analyzer_revision, corpus_revision) = assembled_revisions(revisions);
    PackageSearchPages {
        items,
        warnings,
        analyzer_revision,
        corpus_revision,
    }
}

fn symbol_pages(
    items: Vec<PackageSymbol>,
    warnings: Vec<Warning>,
    revisions: Option<(String, String)>,
) -> PackageSymbolPages {
    let (analyzer_revision, corpus_revision) = assembled_revisions(revisions);
    PackageSymbolPages {
        items,
        warnings,
        analyzer_revision,
        corpus_revision,
    }
}

fn search_hit_identity(hit: &PackageSearchHit) -> (String, String, String, String) {
    let symbol_id = match &hit.symbol.id {
        Some(value) => value.clone(),
        None => hit.symbol.name.clone(),
    };
    (
        hit.package.manager.clone(),
        hit.package.name.clone(),
        hit.package.version.clone(),
        format!("{}:{symbol_id}", hit.unit),
    )
}

fn symbol_hit_identity(hit: &PackageSymbol) -> (String, String, String, String) {
    let symbol_id = match &hit.symbol.id {
        Some(value) => value.clone(),
        None => hit.symbol.name.clone(),
    };
    (
        hit.package.manager.clone(),
        hit.package.name.clone(),
        hit.package.version.clone(),
        format!("{}:{symbol_id}", hit.unit),
    )
}

fn validate_config(config: &Config) -> Result<(), ConfigError> {
    if !(Duration::from_millis(100)..=Duration::from_secs(30)).contains(&config.connect_timeout) {
        return Err(ConfigError::OutOfRange("connect_timeout"));
    }
    if !(Duration::from_secs(1)..=Duration::from_mins(5)).contains(&config.request_timeout) {
        return Err(ConfigError::OutOfRange("request_timeout"));
    }
    if !(ATTEMPTS_MIN..=ATTEMPTS_MAX).contains(&config.attempts) {
        return Err(ConfigError::OutOfRange("attempts"));
    }
    if !(1..=32).contains(&config.max_in_flight) {
        return Err(ConfigError::OutOfRange("max_in_flight"));
    }
    if !(Duration::from_mins(1)..=Duration::from_hours(24)).contains(&config.capabilities_ttl) {
        return Err(ConfigError::OutOfRange("capabilities_ttl"));
    }
    if !(Duration::from_mins(1)..=Duration::from_hours(24)).contains(&config.resolution_ttl) {
        return Err(ConfigError::OutOfRange("resolution_ttl"));
    }
    if !(Duration::from_secs(1)..=Duration::from_hours(1)).contains(&config.failure_ttl) {
        return Err(ConfigError::OutOfRange("failure_ttl"));
    }
    if config.token_env.is_empty()
        || config
            .token_env
            .bytes()
            .any(|byte| !(byte == b'_' || byte.is_ascii_alphanumeric()))
    {
        return Err(ConfigError::InvalidTokenEnvironment);
    }
    Ok(())
}

fn validate_endpoint(value: &str) -> Result<Url, ConfigError> {
    let endpoint = Url::parse(value).map_err(|_| ConfigError::InvalidEndpoint)?;
    if endpoint.cannot_be_a_base()
        || endpoint.username() != ""
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || endpoint.path().ends_with('/')
        || endpoint.host_str().is_none()
    {
        return Err(ConfigError::InvalidEndpoint);
    }
    if endpoint.scheme() != "https"
        && !(endpoint.scheme() == "http" && endpoint.host_str().is_some_and(is_loopback))
    {
        return Err(ConfigError::InvalidEndpointScheme);
    }
    Ok(endpoint)
}

fn is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

fn read_token(name: &str) -> Result<Option<String>, ConfigError> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(ConfigError::InvalidToken),
        Ok(value) => Ok(Some(value)),
    }
}

fn validate_token(value: String) -> Result<String, ConfigError> {
    if value.is_empty()
        || value.len() > 8 * 1024
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(ConfigError::InvalidToken);
    }
    Ok(value)
}

fn make_url(endpoint: &Url, path: &str) -> Url {
    let mut url = endpoint.clone();
    url.set_path(&format!("{}{}", endpoint.path(), path));
    url
}

fn serialize_body<T: Serialize>(value: &T) -> Result<Vec<u8>, ClientError> {
    let body = serde_json::to_vec(value).map_err(|_| ClientError::Decode { status: 0 })?;
    if body.len() > REQUEST_BODY_BYTES_MAX {
        return Err(ClientError::RequestBodyTooLarge { bytes: body.len() });
    }
    Ok(body)
}

fn validate_body_for_capabilities(
    body: &[u8],
    capabilities: &Capabilities,
) -> Result<(), ClientError> {
    let maximum = smaller_bound(
        capabilities.bounds.request_body_bytes_max,
        REQUEST_BODY_BYTES_MAX,
    );
    if body.len() > maximum {
        return Err(ClientError::RequestBodyTooLarge { bytes: body.len() });
    }
    Ok(())
}

fn header_text(
    headers: &HeaderMap,
    name: impl reqwest::header::AsHeaderName,
    field: &'static str,
) -> Result<Option<String>, ClientError> {
    let value = headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if value
        .as_ref()
        .is_some_and(|value| value.len() > RESPONSE_HEADER_BYTES_MAX)
    {
        return Err(ClientError::InvalidResponseField { field });
    }
    Ok(value)
}
fn is_media(meta: &ResponseMeta, expected: &str) -> bool {
    meta.content_type
        .as_deref()
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case(expected))
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
fn active_response_body_bytes_max(capabilities: &Capabilities) -> usize {
    smaller_bound(
        capabilities.bounds.response_body_bytes_max,
        RESPONSE_BODY_BYTES_MAX,
    )
}

#[cfg(test)]
mod tests;
