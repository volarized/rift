//! `OpenAPI` document for the global package API.

use http::StatusCode;
use rift_protocol::read::{FACETS_SOURCE_BYTES_MAX, MAX_RESULTS_DEFAULT, PAGE_DEFAULT};
use serde_json::{Map, Value, json};

use super::{ENDPOINTS, Endpoint, shared_schemas};

#[path = "schemas.rs"]
mod schemas;

/// Builds the `OpenAPI` document from the client endpoint and protocol definitions.
#[must_use]
pub fn document() -> Value {
    let paths: Map<String, Value> = ENDPOINTS
        .into_iter()
        .map(|endpoint| {
            (
                endpoint.path().to_owned(),
                json!({ (endpoint.method().as_str().to_ascii_lowercase()): operation(endpoint) }),
            )
        })
        .collect();
    json!({
        "openapi": "3.1.0",
        "jsonSchemaDialect": "https://json-schema.org/draft/2020-12/schema",
        "info": {
            "title": "Rift Global Package API",
            "description": "REST contract for package capabilities, resolution, declaration search, and symbol reads. Callers send `Rift-Api-Version: 2` to select versionless identities and revision-based exact reads.",
            "version": "0.0.73",
        },
        "security": security(),
        "paths": paths,
        "components": components(),
    })
}

fn security() -> Value {
    json!([{}, {"bearerAuth": []}])
}

fn components() -> Value {
    json!({
        "securitySchemes": {
            "bearerAuth": {
                "type": "http",
                "scheme": "bearer",
                "bearerFormat": "opaque token",
            },
        },
        "parameters": {
            "Page": {
                "name": "page",
                "in": "query",
                "required": false,
                "description": "The 1-based page to read.",
                "schema": {
                    "type": "integer",
                    "format": "uint64",
                    "minimum": 1,
                    "default": PAGE_DEFAULT,
                },
            },
            "MaxResults": {
                "name": "max_results",
                "in": "query",
                "required": false,
                "description": "Results per page. The server refuses a value above the advertised maximum.",
                "schema": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": crate::PAGE_LIMIT_MAX,
                    "default": MAX_RESULTS_DEFAULT,
                },
            },
        },
        "headers": {
            "ETag": {
                "description": "Entity tag for capability cache validation.",
                "schema": {
                    "description": "Entity tag.",
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 256,
                },
            },
            "Cache-Control": {
                "description": "Private cache policy.",
                "schema": {
                    "description": "Cache policy.",
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 256,
                },
            },
            "WWW-Authenticate": {
                "description": "Bearer authentication challenge.",
                "schema": { "$ref": "#/components/schemas/WWWAuthenticate" },
            },
            "Retry-After": {
                "description": "Bounded retry delay in seconds.",
                "schema": { "$ref": "#/components/schemas/RetryAfter" },
            },
            "RateLimit-Limit": {
                "description": "Bounded request limit for the current period.",
                "schema": { "$ref": "#/components/schemas/RateLimitLimit" },
            },
            "RateLimit-Remaining": {
                "description": "Bounded requests remaining in the current period.",
                "schema": { "$ref": "#/components/schemas/RateLimitRemaining" },
            },
            "RateLimit-Reset": {
                "description": "Unix seconds until the current rate limit resets.",
                "schema": { "$ref": "#/components/schemas/RateLimitReset" },
            },
        },
        "schemas": schemas::schemas(),
    })
}

fn operation(endpoint: Endpoint) -> Value {
    let mut operation = match endpoint {
        Endpoint::Capabilities => capabilities(),
        Endpoint::Resolutions => resolutions(),
        Endpoint::Search => search(),
        Endpoint::Symbols => symbols(),
        Endpoint::Facets => facets(),
        Endpoint::Patterns => patterns(),
        Endpoint::Declarations => declarations(),
    };
    operation["operationId"] = json!(endpoint.operation_id());
    operation["security"] = security();
    let responses: Map<String, Value> = endpoint
        .statuses()
        .into_iter()
        .map(|status| {
            let code = status.as_str();
            let response = operation["responses"]
                .get(code)
                .cloned()
                .unwrap_or_else(|| problem_response(status));
            (code.to_owned(), response)
        })
        .collect();
    operation["responses"] = Value::Object(responses);
    if endpoint == Endpoint::Facets {
        operation["x-rift-page-source-bytes-max"] = json!(FACETS_SOURCE_BYTES_MAX);
    } else {
        operation["x-rift-request-body-bytes-max"] = json!(crate::REQUEST_BODY_BYTES_MAX);
        operation["x-rift-response-body-bytes-max"] = json!(crate::RESPONSE_BODY_BYTES_MAX);
    }
    if endpoint == Endpoint::Patterns {
        operation["x-rift-page-files-max"] = json!(crate::PATTERN_PAGE_FILES_MAX);
        operation["x-rift-page-text-bytes-max"] = json!(8_388_608);
    }
    operation
}

fn problem_response(status: StatusCode) -> Value {
    let description = match status {
        StatusCode::BAD_REQUEST => "Request JSON, query parameters, or field edges are invalid.",
        StatusCode::UNAUTHORIZED => "Authentication is required or credentials are invalid.",
        StatusCode::FORBIDDEN => "Credentials do not permit this operation.",
        StatusCode::NOT_ACCEPTABLE => "The server cannot produce an accepted media type.",
        StatusCode::PAYLOAD_TOO_LARGE => "The request body exceeds the request body bound.",
        StatusCode::UNSUPPORTED_MEDIA_TYPE => "The request media type is unsupported.",
        StatusCode::TOO_MANY_REQUESTS => "The request rate limit is exhausted.",
        StatusCode::INTERNAL_SERVER_ERROR => "The server encountered an unexpected failure.",
        StatusCode::BAD_GATEWAY => "The server received an invalid upstream response.",
        StatusCode::SERVICE_UNAVAILABLE => "The service is temporarily unavailable.",
        StatusCode::GATEWAY_TIMEOUT => "The upstream request timed out.",
        _ => unreachable!("only responses carrying problem details reach this function"),
    };
    let mut response = json!({
        "description": description,
        "content": {
            "application/problem+json": {
                "schema": { "$ref": "#/components/schemas/ProblemDetails" },
            },
        },
    });
    match status {
        StatusCode::UNAUTHORIZED => {
            response["headers"] = json!({
                "WWW-Authenticate": {
                    "schema": { "$ref": "#/components/schemas/WWWAuthenticate" },
                },
            });
        }
        StatusCode::TOO_MANY_REQUESTS => {
            response["headers"] = json!({
                "Retry-After": {
                    "schema": { "$ref": "#/components/schemas/RetryAfter" },
                },
                "RateLimit-Limit": {
                    "schema": { "$ref": "#/components/schemas/RateLimitLimit" },
                },
                "RateLimit-Remaining": {
                    "schema": { "$ref": "#/components/schemas/RateLimitRemaining" },
                },
                "RateLimit-Reset": {
                    "schema": { "$ref": "#/components/schemas/RateLimitReset" },
                },
            });
        }
        StatusCode::SERVICE_UNAVAILABLE => {
            response["headers"] = json!({
                "Retry-After": {
                    "schema": { "$ref": "#/components/schemas/RetryAfter" },
                },
            });
        }
        _ => {}
    }
    response
}

fn capabilities() -> Value {
    json!({
        "summary": "Read package API capabilities.",
        "description": "Returns supported package managers, features, publication revisions, and active bounds.",
        "responses": {
            "200": {
                "description": "Capabilities and active bounds.",
                "content": {
                    "application/json": {
                        "schema": { "$ref": "#/components/schemas/Capabilities" },
                        "examples": {
                            "valid": capabilities_200_valid(),
                        },
                    },
                },
                "headers": {
                    "ETag": { "$ref": "#/components/headers/ETag" },
                    "Cache-Control": { "$ref": "#/components/headers/Cache-Control" },
                },
            },
            "304": {
                "description": "The cached capability representation has not changed.",
                "headers": {
                    "ETag": { "$ref": "#/components/headers/ETag" },
                    "Cache-Control": { "$ref": "#/components/headers/Cache-Control" },
                },
            },
        },
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "the operation specifies complete wire examples"
)]
fn resolutions() -> Value {
    json!({
        "summary": "Resolve package context.",
        "description": "Answers each canonical entry with a collected release of its package. An exact version answers with that release and a requirement with the newest release it admits; when the package holds no such release, the entry answers with the release nearest the version it names, for a requirement the lowest version it admits. An entry whose package holds no collected release answers as missing.",
        "requestBody": {
            "description": "Canonical package context.",
            "required": true,
            "content": {
                "application/json": {
                    "schema": { "$ref": "#/components/schemas/PackageResolutionRequest" },
                    "examples": {
                        "valid": {
                            "summary": "Valid request",
                            "value": {
                                "entries": [
                                    {
                                        "manager": "cargo",
                                        "name": "serde",
                                        "availability": "canonical",
                                        "version": "1.0.219",
                                        "registry": "crates.io",
                                    },
                                    {
                                        "manager": "npm",
                                        "name": "typescript",
                                        "availability": "canonical",
                                        "requirement": "^5.7.2",
                                        "registry": "npmjs.org",
                                    },
                                ],
                            },
                        },
                        "nearest": {
                            "summary": "Entries no collected release answers exactly",
                            "value": {
                                "entries": [
                                    {
                                        "manager": "npm",
                                        "name": "typescript",
                                        "availability": "canonical",
                                        "requirement": "~5.7.2",
                                        "registry": "npmjs.org",
                                    },
                                    {
                                        "manager": "pypi",
                                        "name": "sqlalchemy",
                                        "availability": "canonical",
                                        "version": "2.0.30",
                                        "registry": "pypi.org",
                                    },
                                ],
                            },
                        },
                    },
                },
            },
        },
        "responses": {
            "200": {
                "description": "Resolution result accounting for every submitted entry.",
                "content": {
                    "application/json": {
                        "schema": { "$ref": "#/components/schemas/PackageResolutionResponse" },
                        "examples": {
                            "valid": {
                                "summary": "Valid response",
                                "value": {
                                    "available_exact": [
                                        {
                                            "manager": "cargo",
                                            "registry": "crates.io",
                                            "name": "serde",
                                            "version": "1.0.219",
                                        },
                                    ],
                                    "resolved_requirements": [
                                        {
                                            "entry": {
                                                "manager": "npm",
                                                "name": "typescript",
                                                "registry": "npmjs.org",
                                                "requirement": "^5.7.2",
                                                "availability": "canonical",
                                            },
                                            "package": {
                                                "manager": "npm",
                                                "registry": "npmjs.org",
                                                "name": "typescript",
                                                "version": "5.9.3",
                                            },
                                        },
                                    ],
                                    "missing_exact": [],
                                    "missing_requirements": [],
                                },
                            },
                            "nearest": {
                                "summary": "Answers from the nearest collected release",
                                "value": {
                                    "available_exact": [],
                                    "resolved_requirements": [
                                        {
                                            "entry": {
                                                "manager": "npm",
                                                "name": "typescript",
                                                "registry": "npmjs.org",
                                                "requirement": "~5.7.2",
                                                "availability": "canonical",
                                            },
                                            "package": {
                                                "manager": "npm",
                                                "registry": "npmjs.org",
                                                "name": "typescript",
                                                "version": "5.7.3",
                                            },
                                        },
                                        {
                                            "entry": {
                                                "manager": "pypi",
                                                "name": "sqlalchemy",
                                                "registry": "pypi.org",
                                                "version": "2.0.30",
                                                "availability": "canonical",
                                            },
                                            "package": {
                                                "manager": "pypi",
                                                "registry": "pypi.org",
                                                "name": "sqlalchemy",
                                                "version": "2.0.48",
                                            },
                                        },
                                    ],
                                    "missing_exact": [],
                                    "missing_requirements": [],
                                },
                            },
                        },
                    },
                },
            },
        },
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "the operation specifies complete wire examples"
)]
fn search() -> Value {
    json!({
        "summary": "Search package declarations.",
        "description": "Returns one page of results from the selected exact package versions. The caller pins later pages with the returned `revision`.",
        "parameters": [
            { "$ref": "#/components/parameters/Page" },
            { "$ref": "#/components/parameters/MaxResults" },
        ],
        "requestBody": {
            "description": "Structured package search query.",
            "required": true,
            "content": {
                "application/json": {
                    "schema": {
                        "oneOf": [
                            { "$ref": "#/components/schemas/PackageSearchRequest" },
                            { "$ref": "#/components/schemas/EdgeSearchRequest" },
                        ],
                    },
                    "examples": {
                        "valid": {
                            "summary": "Valid request",
                            "value": {
                                "query": "Buffer",
                                "terms": [
                                    {
                                        "text": "Buffer",
                                        "phrase": false,
                                        "prefix": false,
                                    },
                                ],
                                "identifiers": ["Buffer"],
                                "packages": [
                                    {
                                        "manager": "cargo",
                                        "name": "itoa",
                                        "version": "1.0.18",
                                        "registry": "crates.io",
                                    },
                                ],
                                "phase": "precise",
                            },
                        },
                        "documentation": {
                            "summary": "Documentation search request",
                            "value": {
                                "query": "itoa",
                                "terms": [
                                    {
                                        "text": "itoa",
                                        "phrase": false,
                                        "prefix": false,
                                    },
                                ],
                                "identifiers": ["itoa"],
                                "packages": [
                                    {
                                        "manager": "cargo",
                                        "name": "itoa",
                                        "version": "1.0.18",
                                        "registry": "crates.io",
                                    },
                                ],
                                "phase": "precise",
                                "target": "documentation",
                            },
                        },
                        "edges": {
                            "summary": "Exact edge request",
                            "value": {
                                "edges": {
                                    "seeds": ["rift://symbol/pypi/pypi.org/idna/python/idna/codec/Codec"],
                                    "direction": "outgoing",
                                    "kinds": ["contains"],
                                },
                                "packages": [
                                    {
                                        "manager": "pypi",
                                        "name": "idna",
                                        "version": "3.12",
                                        "registry": "pypi.org",
                                    },
                                ],
                            },
                        },
                    },
                },
            },
        },
        "responses": {
            "200": {
                "description": "One ranked package declaration page or one complete edge page.",
                "content": {
                    "application/json": {
                        "schema": {
                            "oneOf": [
                                { "$ref": "#/components/schemas/PackageSearchPage" },
                                { "$ref": "#/components/schemas/EdgeSearchPage" },
                            ],
                        },
                        "examples": {
                            "valid": search_200_valid(),
                            "documentation": search_200_documentation(),
                            "edges": search_200_edges(),
                        },
                    },
                },
            },
            "503": {
                "description": "The service is temporarily unavailable.",
                "content": {
                    "application/problem+json": {
                        "schema": { "$ref": "#/components/schemas/ProblemDetails" },
                    },
                    "application/json": {
                        "schema": { "$ref": "#/components/schemas/SearchUnavailable" },
                        "examples": {
                            "insufficient_coverage": {
                                "summary": "Required edge coverage is unavailable",
                                "value": {
                                    "outcome": "unavailable",
                                    "reason": "insufficient_coverage",
                                },
                            },
                        },
                    },
                },
                "headers": {
                    "Retry-After": {
                        "schema": { "$ref": "#/components/schemas/RetryAfter" },
                    },
                },
            },
        },
    })
}

fn symbols() -> Value {
    json!({
        "summary": "Read one logical symbol.",
        "description": "Reads one exact logical symbol obtained from discovery or another read. The caller selects declarations with `page` and `max_results`, and pins later pages with `revision`. A `found` answer to a request whose `include.history` is `true` **MUST** carry `history`: the collected releases of the symbol's owner that introduced, changed, or removed its declaration, as `introduced_in_release`, `changed_in_release`, and `removed_in_release` edges, newest release first. The server answers `unavailable` with reason `insufficient_coverage` when it cannot establish that history.",
        "requestBody": {
            "description": "Reads one exact logical symbol obtained from discovery or another read.",
            "required": true,
            "content": {
                "application/json": {
                    "schema": { "$ref": "#/components/schemas/GetSymbolParams" },
                    "examples": {
                        "exact": {
                            "value": {
                                "id": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer",
                                "revision": "rift://revision/cargo/crates.io/itoa@1.0.18",
                                "max_results": 5,
                            },
                        },
                    },
                },
            },
        },
        "responses": {
            "200": {
                "description": "One exact logical symbol.",
                "content": {
                    "application/json": {
                        "schema": { "$ref": "#/components/schemas/GetSymbolResult" },
                        "examples": {
                            "exact": symbols_200_exact(),
                        },
                    },
                },
            },
            "503": {
                "description": "Typed selection unavailability or service failure.",
                "content": {
                    "application/problem+json": {
                        "schema": { "$ref": "#/components/schemas/ProblemDetails" },
                    },
                    "application/json": {
                        "schema": { "$ref": "#/components/schemas/GetSymbolResult" },
                    },
                },
                "headers": {
                    "Retry-After": {
                        "schema": { "$ref": "#/components/schemas/RetryAfter" },
                    },
                },
            },
            "404": {
                "description": "The valid identity is absent from the completely searched exact selection.",
                "content": {
                    "application/json": {
                        "schema": { "$ref": "#/components/schemas/GetSymbolResult" },
                    },
                },
            },
        },
    })
}

fn facets() -> Value {
    json!({
        "summary": "Read every declaration of one package or runtime symbol.",
        "description": "Reads every declaration of one package or runtime symbol with its location, exact source, and syntax structure. The caller selects declarations with `page` and `max_results`, and pins later pages with `revision`.",
        "requestBody": {
            "description": "The package or runtime symbol whose facets the request reads.",
            "required": true,
            "content": {
                "application/json": {
                    "schema": { "$ref": "#/components/schemas/FacetsParams" },
                    "examples": {
                        "symbol": {
                            "value": {
                                "symbol": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer",
                                "revision": "rift://revision/cargo/crates.io/itoa@1.0.18",
                            },
                        },
                    },
                },
            },
        },
        "responses": {
            "200": {
                "description": "The symbol's declarations with exact source and syntax structure.",
                "content": {
                    "application/json": {
                        "schema": { "$ref": "#/components/schemas/FacetsResult" },
                        "examples": {
                            "symbol": facets_200_symbol(),
                        },
                    },
                },
            },
            "503": {
                "description": "Typed selection unavailability or service failure.",
                "content": {
                    "application/problem+json": {
                        "schema": { "$ref": "#/components/schemas/ProblemDetails" },
                    },
                },
                "headers": {
                    "Retry-After": {
                        "schema": { "$ref": "#/components/schemas/RetryAfter" },
                    },
                },
            },
            "404": {
                "description": "The release the symbol names declares no such symbol.",
                "content": {
                    "application/problem+json": {
                        "schema": { "$ref": "#/components/schemas/ProblemDetails" },
                    },
                },
            },
        },
    })
}

fn patterns() -> Value {
    json!({
        "summary": "Search package source for a pattern.",
        "description": "Returns one page of results from the selected exact package versions. The caller pins later pages with the returned `revision`.",
        "parameters": [
            { "$ref": "#/components/parameters/Page" },
            { "$ref": "#/components/parameters/MaxResults" },
        ],
        "requestBody": {
            "description": "Pattern and selected package versions.",
            "required": true,
            "content": {
                "application/json": {
                    "schema": { "$ref": "#/components/schemas/PackagePatternRequest" },
                    "examples": {
                        "valid": {
                            "summary": "Valid request",
                            "value": {
                                "pattern": "pub fn format",
                                "packages": [
                                    {
                                        "manager": "cargo",
                                        "name": "itoa",
                                        "version": "1.0.18",
                                        "registry": "crates.io",
                                    },
                                ],
                                "include": {
                                    "source": true,
                                },
                            },
                        },
                    },
                },
            },
        },
        "responses": {
            "200": {
                "description": "One page of pattern matches.",
                "content": {
                    "application/json": {
                        "schema": { "$ref": "#/components/schemas/PackagePatternPage" },
                        "examples": {
                            "valid": patterns_200_valid(),
                        },
                    },
                },
            },
        },
    })
}

fn declarations() -> Value {
    json!({
        "summary": "Find declarations holding source positions.",
        "description": "Finds declarations holding source positions at the releases the positions name. Answers each position once, retaining request order. A position in a type stub may name a declaration with a proved runtime or package owner. Complete facts of a position's release with no declaration there return missing; unavailable coverage returns unavailable. The server MUST NOT answer from another version, and MUST NOT move a position past its line or the text, or one inside a character, to a neighboring one.",
        "requestBody": {
            "description": "Source positions to answer, each at the release it names.",
            "required": true,
            "content": {
                "application/json": {
                    "schema": { "$ref": "#/components/schemas/FindDeclarationsParams" },
                    "examples": {
                        "valid": {
                            "summary": "Valid request",
                            "value": {
                                "position_encoding": "utf-16",
                                "positions": [
                                    {
                                        "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
                                        "revision": "rift://revision/cargo/crates.io/itoa@1.0.18",
                                        "line": 71,
                                        "character": 11,
                                    },
                                    {
                                        "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
                                        "revision": "rift://revision/cargo/crates.io/itoa@1.0.18",
                                        "line": 0,
                                        "character": 0,
                                    },
                                ],
                            },
                        },
                    },
                },
            },
        },
        "responses": {
            "200": {
                "description": "Ordered outcomes for every submitted position.",
                "content": {
                    "application/json": {
                        "schema": { "$ref": "#/components/schemas/FindDeclarationsResult" },
                        "examples": {
                            "valid": {
                                "summary": "Valid response",
                                "value": {
                                    "results": [
                                        {
                                            "position": {
                                                "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
                                                "revision": "rift://revision/cargo/crates.io/itoa@1.0.18",
                                                "line": 71,
                                                "character": 11,
                                            },
                                            "id": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer",
                                            "kind": "struct",
                                            "revision": "rift://revision/cargo/crates.io/itoa@1.0.18",
                                            "outcome": "found",
                                        },
                                        {
                                            "position": {
                                                "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
                                                "revision": "rift://revision/cargo/crates.io/itoa@1.0.18",
                                                "line": 0,
                                                "character": 0,
                                            },
                                            "outcome": "missing",
                                        },
                                    ],
                                },
                            },
                        },
                    },
                },
            },
        },
    })
}

fn capabilities_200_valid_value() -> Value {
    json!({
        "supported_package_managers": ["cargo", "npm", "pypi"],
        "supported_features": [
            "resolutions",
            "search",
            "symbols",
            "documentation_search",
            "symbol_documentation",
            "patterns",
            "declarations",
            "facets",
            "edges",
        ],
        "publication_format": "rift-package-index-v2",
        "analyzer_revision": "8f3371e3",
        "corpus_revision": "9ed97ad7",
        "required_search_fields": ["name", "qualified_name", "documentation", "signature"],
        "bounds": {
            "request_body_bytes_max": 4_194_304,
            "response_body_bytes_max": 33_554_432,
            "dependency_entries_max": 20_000,
            "query_bytes_max": 4096,
            "query_terms_max": 32,
            "query_term_bytes_max": 256,
            "identifiers_max": 16,
            "identifier_bytes_max": 256,
            "packages_max": 64,
            "page_limit_min": 1,
            "page_limit_max": 200,
            "page_limit_default": 10,
            "candidate_pool_max": 1000,
            "warnings_max": 32,
            "source_bytes_max": 33_554_432,
        },
        "documentation_revision": "8f3371e3",
        "edge_capabilities": [
            {
                "direction": "incoming",
                "kinds": ["contains"],
                "bounds": {
                    "max_seeds": 64,
                    "max_kinds": 16,
                    "max_edges": 1000,
                    "max_occurrences": 1000,
                    "max_symbols": 10_000,
                    "max_sources": 10_000,
                },
            },
            {
                "direction": "outgoing",
                "kinds": ["contains"],
                "bounds": {
                    "max_seeds": 64,
                    "max_kinds": 16,
                    "max_edges": 1000,
                    "max_occurrences": 1000,
                    "max_symbols": 10_000,
                    "max_sources": 10_000,
                },
            },
        ],
    })
}

fn capabilities_200_valid() -> Value {
    json!({
        "summary": "Valid response",
        "value": capabilities_200_valid_value(),
    })
}

fn search_200_valid_value_items_1() -> Value {
    json!({
        "package": {
            "manager": "cargo",
            "registry": "crates.io",
            "name": "itoa",
            "version": "1.0.18",
        },
        "symbol": {
            "id": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer",
            "language": "rust",
            "name": "Buffer",
            "kind": "struct",
            "kinds": ["type", "public"],
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "cargo",
                    "registry": "crates.io",
                    "name": "itoa",
                    "version": "1.0.18",
                },
                "source_kind": "authored",
            },
            "modifiers": [],
            "visibility": "pub",
            "types": [],
            "signatures": [],
            "documentation": [
                {
                    "format": "markdown",
                    "text": "A correctly sized stack allocation for the formatted integer to be written\ninto.\n\n# Example\n\n```\nlet mut buffer = itoa::Buffer::new();\nlet printed = buffer.format(1234);\nassert_eq!(printed, \"1234\");\n```",
                },
            ],
            "document_local": false,
        },
        "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
        "range": {
            "start": 2357,
            "end": 2427,
        },
        "line": 72,
        "contributing_fields": ["name", "qualified_name", "documentation", "file_content"],
        "match_class": "qualified_exact",
        "qualified_name": "Buffer",
    })
}

fn search_200_valid_value_items_2() -> Value {
    json!({
        "package": {
            "manager": "cargo",
            "registry": "crates.io",
            "name": "itoa",
            "version": "1.0.18",
        },
        "symbol": {
            "id": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer/format",
            "language": "rust",
            "name": "format",
            "kind": "function",
            "kinds": ["value", "callable", "public"],
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "cargo",
                    "registry": "crates.io",
                    "name": "itoa",
                    "version": "1.0.18",
                },
                "source_kind": "authored",
            },
            "container": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer",
            "modifiers": [],
            "visibility": "pub",
            "types": [],
            "signatures": [
                {
                    "display": "pub fn format<I: Integer>(&mut self, i: I) -> &str",
                    "links": [],
                    "language": "rust",
                    "parameters": [],
                    "returns": [],
                    "type_parameters": [],
                    "throws": [],
                    "effects": [],
                },
            ],
            "documentation": [
                {
                    "format": "markdown",
                    "text": "Print an integer into this buffer and return a reference to its string\nrepresentation within the buffer.",
                },
            ],
            "document_local": false,
        },
        "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
        "range": {
            "start": 3181,
            "end": 3484,
        },
        "line": 106,
        "contributing_fields": ["qualified_name", "documentation", "file_content"],
        "match_class": "substring",
        "qualified_name": "Buffer::format",
    })
}

fn search_200_valid_value_items_3() -> Value {
    json!({
        "package": {
            "manager": "cargo",
            "registry": "crates.io",
            "name": "itoa",
            "version": "1.0.18",
        },
        "symbol": {
            "id": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer/new",
            "language": "rust",
            "name": "new",
            "kind": "function",
            "kinds": ["value", "callable", "public"],
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "cargo",
                    "registry": "crates.io",
                    "name": "itoa",
                    "version": "1.0.18",
                },
                "source_kind": "authored",
            },
            "container": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer",
            "modifiers": [],
            "visibility": "pub",
            "types": [],
            "signatures": [
                {
                    "display": "pub fn new() -> Buffer",
                    "links": [],
                    "language": "rust",
                    "parameters": [],
                    "returns": [],
                    "type_parameters": [],
                    "throws": [],
                    "effects": [],
                },
            ],
            "documentation": [
                {
                    "format": "markdown",
                    "text": "This is a cheap operation; you don't need to worry about reusing buffers\nfor efficiency.",
                },
            ],
            "document_local": false,
        },
        "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
        "range": {
            "start": 2881,
            "end": 3006,
        },
        "line": 98,
        "contributing_fields": ["qualified_name", "signature", "file_content"],
        "match_class": "substring",
        "qualified_name": "Buffer::new",
    })
}

fn search_200_valid_value_items_4() -> Value {
    json!({
        "package": {
            "manager": "cargo",
            "registry": "crates.io",
            "name": "itoa",
            "version": "1.0.18",
        },
        "symbol": {
            "id": "rift://symbol/cargo/crates.io/itoa/rust/itoa/private/Sealed/write",
            "language": "rust",
            "name": "write",
            "kind": "function",
            "kinds": ["value", "callable"],
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "cargo",
                    "registry": "crates.io",
                    "name": "itoa",
                    "version": "1.0.18",
                },
                "source_kind": "authored",
            },
            "container": "rift://symbol/cargo/crates.io/itoa/rust/itoa/private/Sealed",
            "modifiers": [],
            "visibility": "private",
            "types": [],
            "signatures": [
                {
                    "display": "fn write(self, buf: &mut Self::Buffer) -> &str",
                    "links": [],
                    "language": "rust",
                    "parameters": [],
                    "returns": [],
                    "type_parameters": [],
                    "throws": [],
                    "effects": [],
                },
            ],
            "documentation": [],
            "document_local": false,
        },
        "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
        "range": {
            "start": 4029,
            "end": 4076,
        },
        "line": 131,
        "contributing_fields": ["signature", "file_content"],
        "match_class": "unknown",
        "qualified_name": "private::Sealed::write",
    })
}

fn search_200_valid_value_items_5() -> Value {
    json!({
        "package": {
            "manager": "cargo",
            "registry": "crates.io",
            "name": "itoa",
            "version": "1.0.18",
        },
        "symbol": {
            "id": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Integer",
            "language": "rust",
            "name": "Integer",
            "kind": "trait",
            "kinds": ["type", "public"],
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "cargo",
                    "registry": "crates.io",
                    "name": "itoa",
                    "version": "1.0.18",
                },
                "source_kind": "authored",
            },
            "modifiers": [],
            "visibility": "pub",
            "types": [],
            "signatures": [],
            "documentation": [
                {
                    "format": "markdown",
                    "text": "An integer that can be written into an [`itoa::Buffer`][Buffer].\n\nThis trait is sealed and cannot be implemented for types outside of itoa.",
                },
            ],
            "document_local": false,
        },
        "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
        "range": {
            "start": 3639,
            "end": 3836,
        },
        "line": 119,
        "contributing_fields": ["documentation"],
        "match_class": "unknown",
        "qualified_name": "Integer",
    })
}

fn search_200_valid_value_items_6() -> Value {
    json!({
        "package": {
            "manager": "cargo",
            "registry": "crates.io",
            "name": "itoa",
            "version": "1.0.18",
        },
        "symbol": {
            "id": "rift://symbol/cargo/crates.io/itoa/rust/itoa/private/Sealed",
            "language": "rust",
            "name": "Sealed",
            "kind": "trait",
            "kinds": ["type", "public"],
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "cargo",
                    "registry": "crates.io",
                    "name": "itoa",
                    "version": "1.0.18",
                },
                "source_kind": "authored",
            },
            "container": "rift://symbol/cargo/crates.io/itoa/rust/itoa/private",
            "modifiers": [],
            "visibility": "pub",
            "types": [],
            "signatures": [],
            "documentation": [],
            "document_local": false,
        },
        "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
        "range": {
            "start": 3943,
            "end": 4082,
        },
        "line": 128,
        "contributing_fields": ["file_content"],
        "match_class": "unknown",
        "qualified_name": "private::Sealed",
    })
}

fn search_200_valid_value_items() -> Value {
    json!([
        search_200_valid_value_items_1(),
        search_200_valid_value_items_2(),
        search_200_valid_value_items_3(),
        search_200_valid_value_items_4(),
        search_200_valid_value_items_5(),
        search_200_valid_value_items_6(),
    ])
}

fn search_200_valid_value() -> Value {
    json!({
        "items": search_200_valid_value_items(),
        "warnings": [],
        "publication_format": "rift-package-index-v2",
        "analyzer_revision": "8f3371e3",
        "corpus_revision": "9ed97ad7",
        "documentation_revision": "8f3371e3",
        "view": {
            "id": "eyJ2ZXJzaW9uIjo0LCJzbG90IjoxMTcsImRlc2NyaXB0b3JfZGlnZXN0IjoiOTA0ZmE0YzJiYWMyYjUwNjQ5OGJlYjZhMGRiYmZmZmQ2OTdhM2I3MWNjOWI5MzhiZGM4NDdhYjgzZWM3M2NjMiIsImNvbGxlY3Rpb25fY29udGV4dCI6ImQyZDgyNGIzMjA5NTczNDM4M2U4MGZmMmY3ZjRjNzQzNmRiZGM0NDFkM2RlOTZlNGMxNzlkMjljMjkzM2JlOTIiLCJleHBpcmVzX2F0IjoxNzk0OTE4NTUwfQ.l1PfACRL2gkCmojWRbbDLf6N28_dqya4hwfn5Yp2tvU",
            "expires_at": "2026-11-17T12:29:10Z",
        },
        "pagination": {
            "page": 1,
            "pages": 1,
            "max_results": 10,
            "total": 6,
        },
        "revision": "9ed97ad7",
    })
}

fn search_200_valid() -> Value {
    json!({
        "summary": "Valid response",
        "value": search_200_valid_value(),
    })
}

fn search_200_documentation_value_items_1_documentation() -> Value {
    json!({
        "block": {
            "identity": "46e487b5adde3604c4215057f426a24d62fab95d73378462d7315a36a4b1ef1c",
            "source": {
                "source": {
                    "unit": "rift://source/cargo/crates.io/itoa/README.md",
                    "kind": "package",
                },
            },
            "content_digest": "84c5a4fa3088f8ba8bfc525e77420692fd4420fca76c1cd6223b32cc09792407",
            "heading_path": [
                {
                    "level": 1,
                    "name": "itoa",
                },
            ],
            "range": {
                "start": 0,
                "end": 5,
            },
            "line": 1,
            "kind": "prose",
            "chunks": [
                {
                    "identity": "rift://source/cargo/crates.io/itoa/README.md",
                    "range": {
                        "start": 0,
                        "end": 2299,
                    },
                },
            ],
        },
        "source": {
            "identity": {
                "source": {
                    "unit": "rift://source/cargo/crates.io/itoa/README.md",
                    "kind": "package",
                },
            },
            "revision": "1305bd79675db0194915d9bc91bd0b3cf9487bc1126e05bf46b48ab43edce44b",
            "content_digest": "1305bd79675db0194915d9bc91bd0b3cf9487bc1126e05bf46b48ab43edce44b",
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "cargo",
                    "registry": "crates.io",
                    "name": "itoa",
                    "version": "1.0.18",
                },
                "source_kind": "authored",
            },
            "format": "markdown",
            "media_type": "text/markdown",
            "selection": "package_archive",
            "byte_length": 2299,
        },
        "documentation_revision": "8f3371e3",
    })
}

fn search_200_documentation_value_items_1() -> Value {
    json!({
        "target": "documentation",
        "package": {
            "manager": "cargo",
            "registry": "crates.io",
            "name": "itoa",
            "version": "1.0.18",
        },
        "documentation": search_200_documentation_value_items_1_documentation(),
        "contributing_fields": ["content"],
    })
}

fn search_200_documentation_value_items_2_documentation() -> Value {
    json!({
        "block": {
            "identity": "f663ab705633739b8337b6bf483ada4a06b675b7930fb15f704d3394fff9cfd9",
            "source": {
                "source": {
                    "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
                    "kind": "package",
                },
            },
            "content_digest": "2978679e0059ba28e4bd2ffe908cc5061de407ba86a5f29303c0485e4df2236c",
            "range": {
                "start": 3488,
                "end": 3557,
            },
            "line": 116,
            "kind": "prose",
            "chunks": [
                {
                    "identity": "rift://source/cargo/crates.io/itoa/src/lib.rs",
                    "range": {
                        "start": 0,
                        "end": 16904,
                    },
                },
            ],
            "symbol": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Integer",
        },
        "source": {
            "identity": {
                "source": {
                    "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
                    "kind": "package",
                },
            },
            "revision": "e962338e1886873aa7c3399ec89aa5378ccbfed712b9690f043628079390a34f",
            "content_digest": "e962338e1886873aa7c3399ec89aa5378ccbfed712b9690f043628079390a34f",
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "cargo",
                    "registry": "crates.io",
                    "name": "itoa",
                    "version": "1.0.18",
                },
                "source_kind": "authored",
            },
            "format": "attached_comment",
            "media_type": "text/markdown",
            "selection": "attached_comment",
            "byte_length": 16904,
            "language": "rust",
        },
        "documentation_revision": "8f3371e3",
    })
}

fn search_200_documentation_value_items_2() -> Value {
    json!({
        "target": "documentation",
        "package": {
            "manager": "cargo",
            "registry": "crates.io",
            "name": "itoa",
            "version": "1.0.18",
        },
        "documentation": search_200_documentation_value_items_2_documentation(),
        "contributing_fields": ["documentation"],
    })
}

fn search_200_documentation_value_items_3_documentation() -> Value {
    json!({
        "block": {
            "identity": "85dfa46fdad18c2999e196ad6a0563a89b67e60a521195e1b006aefe89131331",
            "source": {
                "source": {
                    "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
                    "kind": "package",
                },
            },
            "content_digest": "639b0f72b8f659492cd5e727fbad6271312a66d066708fb53ba3e7ab042ca806",
            "range": {
                "start": 2116,
                "end": 2195,
            },
            "line": 62,
            "kind": "prose",
            "chunks": [
                {
                    "identity": "rift://source/cargo/crates.io/itoa/src/lib.rs",
                    "range": {
                        "start": 0,
                        "end": 16904,
                    },
                },
            ],
            "symbol": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer",
        },
        "source": {
            "identity": {
                "source": {
                    "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
                    "kind": "package",
                },
            },
            "revision": "e962338e1886873aa7c3399ec89aa5378ccbfed712b9690f043628079390a34f",
            "content_digest": "e962338e1886873aa7c3399ec89aa5378ccbfed712b9690f043628079390a34f",
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "cargo",
                    "registry": "crates.io",
                    "name": "itoa",
                    "version": "1.0.18",
                },
                "source_kind": "authored",
            },
            "format": "attached_comment",
            "media_type": "text/markdown",
            "selection": "attached_comment",
            "byte_length": 16904,
            "language": "rust",
        },
        "documentation_revision": "8f3371e3",
    })
}

fn search_200_documentation_value_items_3() -> Value {
    json!({
        "target": "documentation",
        "package": {
            "manager": "cargo",
            "registry": "crates.io",
            "name": "itoa",
            "version": "1.0.18",
        },
        "documentation": search_200_documentation_value_items_3_documentation(),
        "contributing_fields": ["documentation", "content"],
    })
}

fn search_200_documentation_value_items() -> Value {
    json!([
        search_200_documentation_value_items_1(),
        search_200_documentation_value_items_2(),
        search_200_documentation_value_items_3(),
    ])
}

fn search_200_documentation_value() -> Value {
    json!({
        "items": search_200_documentation_value_items(),
        "warnings": [],
        "publication_format": "rift-package-index-v2",
        "analyzer_revision": "8f3371e3",
        "corpus_revision": "9ed97ad7",
        "documentation_revision": "8f3371e3",
        "view": {
            "id": "eyJ2ZXJzaW9uIjo0LCJzbG90IjoxMTcsImRlc2NyaXB0b3JfZGlnZXN0IjoiOTA0ZmE0YzJiYWMyYjUwNjQ5OGJlYjZhMGRiYmZmZmQ2OTdhM2I3MWNjOWI5MzhiZGM4NDdhYjgzZWM3M2NjMiIsImNvbGxlY3Rpb25fY29udGV4dCI6ImQyZDgyNGIzMjA5NTczNDM4M2U4MGZmMmY3ZjRjNzQzNmRiZGM0NDFkM2RlOTZlNGMxNzlkMjljMjkzM2JlOTIiLCJleHBpcmVzX2F0IjoxNzk0OTE4NTUwfQ.l1PfACRL2gkCmojWRbbDLf6N28_dqya4hwfn5Yp2tvU",
            "expires_at": "2026-11-17T12:29:10Z",
        },
        "pagination": {
            "page": 1,
            "pages": 1,
            "max_results": 10,
            "total": 3,
        },
        "revision": "9ed97ad7",
    })
}

fn search_200_documentation() -> Value {
    json!({
        "summary": "Documentation search response",
        "value": search_200_documentation_value(),
    })
}

fn search_200_edges_value_symbols_1() -> Value {
    json!({
        "symbol": {
            "id": "rift://symbol/pypi/pypi.org/idna/python/idna/codec/Codec",
            "language": "python",
            "name": "Codec",
            "kind": "class",
            "kinds": ["type"],
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "pypi",
                    "registry": "pypi.org",
                    "name": "idna",
                    "version": "3.12",
                },
                "source_kind": "authored",
            },
        },
        "declarations": [
            {
                "origin": {
                    "location": "dependency",
                    "package": {
                        "manager": "pypi",
                        "registry": "pypi.org",
                        "name": "idna",
                        "version": "3.12",
                    },
                    "source_kind": "authored",
                },
                "unit": "rift://source/pypi/pypi.org/idna/idna/codec.py",
                "range": {
                    "start": 188,
                    "end": 764,
                },
                "line": 10,
                "signature_indices": [],
                "type_indices": [],
                "documentation_indices": [],
            },
        ],
    })
}

fn search_200_edges_value_symbols_2() -> Value {
    json!({
        "symbol": {
            "id": "rift://symbol/pypi/pypi.org/idna/python/idna/codec/Codec/decode",
            "language": "python",
            "name": "decode",
            "kind": "function",
            "kinds": ["value", "callable"],
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "pypi",
                    "registry": "pypi.org",
                    "name": "idna",
                    "version": "3.12",
                },
                "source_kind": "authored",
            },
            "container": "rift://symbol/pypi/pypi.org/idna/python/idna/codec/Codec",
            "signatures": [
                {
                    "display": "def decode(self, data: bytes, errors: str = \"strict\") -> Tuple[str, int]:",
                    "language": "python",
                },
            ],
        },
        "declarations": [
            {
                "origin": {
                    "location": "dependency",
                    "package": {
                        "manager": "pypi",
                        "registry": "pypi.org",
                        "name": "idna",
                        "version": "3.12",
                    },
                    "source_kind": "authored",
                },
                "unit": "rift://source/pypi/pypi.org/idna/idna/codec.py",
                "range": {
                    "start": 495,
                    "end": 764,
                },
                "line": 20,
                "signature_indices": [0],
                "type_indices": [],
                "documentation_indices": [],
            },
        ],
    })
}

fn search_200_edges_value_symbols_3() -> Value {
    json!({
        "symbol": {
            "id": "rift://symbol/pypi/pypi.org/idna/python/idna/codec/Codec/encode",
            "language": "python",
            "name": "encode",
            "kind": "function",
            "kinds": ["value", "callable"],
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "pypi",
                    "registry": "pypi.org",
                    "name": "idna",
                    "version": "3.12",
                },
                "source_kind": "authored",
            },
            "container": "rift://symbol/pypi/pypi.org/idna/python/idna/codec/Codec",
            "signatures": [
                {
                    "display": "def encode(self, data: str, errors: str = \"strict\") -> Tuple[bytes, int]:",
                    "language": "python",
                },
            ],
        },
        "declarations": [
            {
                "origin": {
                    "location": "dependency",
                    "package": {
                        "manager": "pypi",
                        "registry": "pypi.org",
                        "name": "idna",
                        "version": "3.12",
                    },
                    "source_kind": "authored",
                },
                "unit": "rift://source/pypi/pypi.org/idna/idna/codec.py",
                "range": {
                    "start": 219,
                    "end": 489,
                },
                "line": 11,
                "signature_indices": [0],
                "type_indices": [],
                "documentation_indices": [],
            },
        ],
    })
}

fn search_200_edges_value_symbols() -> Value {
    json!([
        search_200_edges_value_symbols_1(),
        search_200_edges_value_symbols_2(),
        search_200_edges_value_symbols_3(),
    ])
}

fn search_200_edges_value() -> Value {
    json!({
        "groups": [
            {
                "seed": "rift://symbol/pypi/pypi.org/idna/python/idna/codec/Codec",
                "edges": [
                    {
                        "id": "63d8dd9d4533c338f4f1d9953fad4eab6b92b5723a55f88b888fe9abfca1d9dd",
                        "edge": {
                            "from": "rift://symbol/pypi/pypi.org/idna/python/idna/codec/Codec",
                            "kind": "contains",
                            "kinds": ["contains"],
                            "to": "rift://symbol/pypi/pypi.org/idna/python/idna/codec/Codec/decode",
                            "derivation": "resolution",
                            "producer": {
                                "name": "canonical-containment",
                                "revision": "1",
                            },
                        },
                    },
                    {
                        "id": "b883e8af769222c814e984fc66de5d04385b52127d77e0953d48fdae4837048f",
                        "edge": {
                            "from": "rift://symbol/pypi/pypi.org/idna/python/idna/codec/Codec",
                            "kind": "contains",
                            "kinds": ["contains"],
                            "to": "rift://symbol/pypi/pypi.org/idna/python/idna/codec/Codec/encode",
                            "derivation": "resolution",
                            "producer": {
                                "name": "canonical-containment",
                                "revision": "1",
                            },
                        },
                    },
                ],
                "complete": true,
            },
        ],
        "symbols": search_200_edges_value_symbols(),
        "sources": [
            {
                "unit": "rift://source/pypi/pypi.org/idna/idna/codec.py",
                "digest": "33648658dedcb3fe81df6426293c9337ac50199798420436c225f7fc347a9681",
            },
        ],
        "analyzer_revision": "8f3371e3",
        "corpus_revision": "9ed97ad7",
        "view": {
            "id": "eyJ2ZXJzaW9uIjo0LCJzbG90Ijo0MiwiZGVzY3JpcHRvcl9kaWdlc3QiOiI1OWMxOTFlZjJlNWNiYmFkNDQ2ZWMzODc3ZGVhYjg4ZGNkODU4MmVmZGI4ZWI4MDExMTVkYzFmN2ZiODA2MjEzIiwiY29sbGVjdGlvbl9jb250ZXh0IjoiZDJkODI0YjMyMDk1NzM0MzgzZTgwZmYyZjdmNGM3NDM2ZGJkYzQ0MWQzZGU5NmU0YzE3OWQyOWMyOTMzYmU5MiIsImV4cGlyZXNfYXQiOjE3OTQ5MTg5NzN9.zXcYE4XqU0zS0KCFfZXH4cWmq8hvIltc2uMea-5Pu8s",
            "expires_at": "2026-11-17T12:36:13Z",
        },
    })
}

fn search_200_edges() -> Value {
    json!({
        "summary": "Edge page",
        "value": search_200_edges_value(),
    })
}

fn symbols_200_exact_value() -> Value {
    json!({
        "outcome": "found",
        "symbol": {
            "id": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer",
            "language": "rust",
            "name": "Buffer",
            "kind": "struct",
            "kinds": ["type", "public"],
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "cargo",
                    "registry": "crates.io",
                    "name": "itoa",
                    "version": "1.0.18",
                },
                "source_kind": "authored",
            },
            "visibility": "pub",
            "documentation": [
                {
                    "format": "markdown",
                    "text": "A correctly sized stack allocation for the formatted integer to be written\ninto.\n\n# Example\n\n```\nlet mut buffer = itoa::Buffer::new();\nlet printed = buffer.format(1234);\nassert_eq!(printed, \"1234\");\n```",
                },
            ],
        },
        "revision": "rift://revision/cargo/crates.io/itoa@1.0.18",
        "declarations": [
            {
                "origin": {
                    "location": "dependency",
                    "package": {
                        "manager": "cargo",
                        "registry": "crates.io",
                        "name": "itoa",
                        "version": "1.0.18",
                    },
                    "source_kind": "authored",
                },
                "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
                "range": {
                    "start": 2357,
                    "end": 2427,
                },
                "line": 72,
                "signature_indices": [],
                "type_indices": [],
                "documentation_indices": [0],
            },
        ],
        "pagination": {
            "page": 1,
            "pages": 1,
            "max_results": 10,
            "total": 1,
        },
    })
}

fn symbols_200_exact() -> Value {
    json!({
        "value": symbols_200_exact_value(),
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "the operation specifies complete wire examples"
)]
fn facets_200_symbol_value_declarations_1_syntax() -> Value {
    json!([
        {
            "declaration": 0,
            "index": 0,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "struct_item",
            "kinds": ["declaration", "definition"],
            "range": {
                "start": 2357,
                "end": 2427,
            },
            "source": "pub struct Buffer {\n    bytes: [MaybeUninit<u8>; i128::MAX_STR_LEN],\n}",
            "regions": [],
        },
        {
            "declaration": 0,
            "index": 1,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "visibility_modifier",
            "kinds": [],
            "range": {
                "start": 2357,
                "end": 2360,
            },
            "source": "pub",
            "regions": [],
            "parent": 0,
        },
        {
            "declaration": 0,
            "index": 2,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "type_identifier",
            "kinds": [],
            "range": {
                "start": 2368,
                "end": 2374,
            },
            "source": "Buffer",
            "regions": [],
            "parent": 0,
        },
        {
            "declaration": 0,
            "index": 3,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "field_declaration_list",
            "kinds": [],
            "range": {
                "start": 2375,
                "end": 2427,
            },
            "source": "{\n    bytes: [MaybeUninit<u8>; i128::MAX_STR_LEN],\n}",
            "regions": [],
            "parent": 0,
        },
        {
            "declaration": 0,
            "index": 4,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "field_declaration",
            "kinds": ["declaration", "definition"],
            "range": {
                "start": 2381,
                "end": 2424,
            },
            "source": "bytes: [MaybeUninit<u8>; i128::MAX_STR_LEN]",
            "regions": [],
            "parent": 3,
        },
        {
            "declaration": 0,
            "index": 5,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "field_identifier",
            "kinds": [],
            "range": {
                "start": 2381,
                "end": 2386,
            },
            "source": "bytes",
            "regions": [],
            "parent": 4,
        },
        {
            "declaration": 0,
            "index": 6,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "array_type",
            "kinds": [],
            "range": {
                "start": 2388,
                "end": 2424,
            },
            "source": "[MaybeUninit<u8>; i128::MAX_STR_LEN]",
            "regions": [],
            "parent": 4,
        },
        {
            "declaration": 0,
            "index": 7,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "generic_type",
            "kinds": [],
            "range": {
                "start": 2389,
                "end": 2404,
            },
            "source": "MaybeUninit<u8>",
            "regions": [],
            "parent": 6,
        },
        {
            "declaration": 0,
            "index": 8,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "type_identifier",
            "kinds": [],
            "range": {
                "start": 2389,
                "end": 2400,
            },
            "source": "MaybeUninit",
            "regions": [],
            "parent": 7,
        },
        {
            "declaration": 0,
            "index": 9,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "type_arguments",
            "kinds": [],
            "range": {
                "start": 2400,
                "end": 2404,
            },
            "source": "<u8>",
            "regions": [],
            "parent": 7,
        },
        {
            "declaration": 0,
            "index": 10,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "primitive_type",
            "kinds": [],
            "range": {
                "start": 2401,
                "end": 2403,
            },
            "source": "u8",
            "regions": [],
            "parent": 9,
        },
        {
            "declaration": 0,
            "index": 11,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "scoped_identifier",
            "kinds": [],
            "range": {
                "start": 2406,
                "end": 2423,
            },
            "source": "i128::MAX_STR_LEN",
            "regions": [],
            "parent": 6,
        },
        {
            "declaration": 0,
            "index": 12,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "identifier",
            "kinds": [],
            "range": {
                "start": 2406,
                "end": 2410,
            },
            "source": "i128",
            "regions": [],
            "parent": 11,
        },
        {
            "declaration": 0,
            "index": 13,
            "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
            "language": "rust",
            "kind": "identifier",
            "kinds": [],
            "range": {
                "start": 2412,
                "end": 2423,
            },
            "source": "MAX_STR_LEN",
            "regions": [],
            "parent": 11,
        },
    ])
}

fn facets_200_symbol_value_declarations_1() -> Value {
    json!({
        "declaration": 0,
        "kind": "struct_item",
        "range": {
            "start": 2357,
            "end": 2427,
        },
        "source": {
            "span": {
                "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
                "range": {
                    "start": 2357,
                    "end": 2427,
                },
            },
            "text": "pub struct Buffer {\n    bytes: [MaybeUninit<u8>; i128::MAX_STR_LEN],\n}",
        },
        "syntax": facets_200_symbol_value_declarations_1_syntax(),
        "regions": [],
    })
}

fn facets_200_symbol_value_declarations() -> Value {
    json!([facets_200_symbol_value_declarations_1()])
}

fn facets_200_symbol_value() -> Value {
    json!({
        "symbol": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer",
        "revision": "rift://revision/cargo/crates.io/itoa@1.0.18",
        "warnings": [],
        "declarations": facets_200_symbol_value_declarations(),
        "pagination": {
            "page": 1,
            "pages": 1,
            "max_results": 10,
            "total": 1,
        },
    })
}

fn facets_200_symbol() -> Value {
    json!({
        "value": facets_200_symbol_value(),
    })
}

fn patterns_200_valid_value_items_1_declaration() -> Value {
    json!({
        "symbol": {
            "id": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer/format",
            "language": "rust",
            "name": "format",
            "kind": "function",
            "kinds": ["value", "callable", "public"],
            "origin": {
                "location": "dependency",
                "package": {
                    "manager": "cargo",
                    "registry": "crates.io",
                    "name": "itoa",
                    "version": "1.0.18",
                },
                "source_kind": "authored",
            },
            "container": "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer",
            "modifiers": [],
            "visibility": "pub",
            "types": [],
            "signatures": [
                {
                    "display": "pub fn format<I: Integer>(&mut self, i: I) -> &str",
                    "links": [],
                    "language": "rust",
                    "parameters": [],
                    "returns": [],
                    "type_parameters": [],
                    "throws": [],
                    "effects": [],
                },
            ],
            "documentation": [
                {
                    "format": "markdown",
                    "text": "Print an integer into this buffer and return a reference to its string\nrepresentation within the buffer.",
                },
            ],
            "document_local": false,
        },
        "range": {
            "start": 3181,
            "end": 3484,
        },
        "line": 106,
        "source": "pub fn format<I: Integer>(&mut self, i: I) -> &str {\n        let buf_ptr = self.bytes.as_mut_ptr().cast::<I::Buffer>();\n        let string = i.write(unsafe { &mut *buf_ptr });\n        if string.len() > I::MAX_STR_LEN {\n            unsafe { hint::unreachable_unchecked() };\n        }\n        string\n    }",
    })
}

fn patterns_200_valid_value_items_1() -> Value {
    json!({
        "package": {
            "manager": "cargo",
            "registry": "crates.io",
            "name": "itoa",
            "version": "1.0.18",
        },
        "unit": "rift://source/cargo/crates.io/itoa/src/lib.rs",
        "range": {
            "start": 3181,
            "end": 3194,
        },
        "line": 106,
        "size": 16904,
        "declaration": patterns_200_valid_value_items_1_declaration(),
        "source": "    pub fn format<I: Integer>(&mut self, i: I) -> &str {",
    })
}

fn patterns_200_valid_value_items() -> Value {
    json!([patterns_200_valid_value_items_1()])
}

fn patterns_200_valid_value() -> Value {
    json!({
        "items": patterns_200_valid_value_items(),
        "warnings": [],
        "publication_format": "rift-package-index-v2",
        "analyzer_revision": "8f3371e3",
        "corpus_revision": "9ed97ad7",
        "view": {
            "id": "eyJ2ZXJzaW9uIjo0LCJzbG90IjoxMTcsImRlc2NyaXB0b3JfZGlnZXN0IjoiOTA0ZmE0YzJiYWMyYjUwNjQ5OGJlYjZhMGRiYmZmZmQ2OTdhM2I3MWNjOWI5MzhiZGM4NDdhYjgzZWM3M2NjMiIsImNvbGxlY3Rpb25fY29udGV4dCI6ImQyZDgyNGIzMjA5NTczNDM4M2U4MGZmMmY3ZjRjNzQzNmRiZGM0NDFkM2RlOTZlNGMxNzlkMjljMjkzM2JlOTIiLCJleHBpcmVzX2F0IjoxNzk0OTE4NTUwfQ.l1PfACRL2gkCmojWRbbDLf6N28_dqya4hwfn5Yp2tvU",
            "expires_at": "2026-11-17T12:29:10Z",
        },
        "pagination": {
            "page": 1,
            "pages": 1,
            "max_results": 10,
            "total": 1,
        },
        "revision": "9ed97ad7",
    })
}

fn patterns_200_valid() -> Value {
    json!({
        "summary": "Valid response",
        "value": patterns_200_valid_value(),
    })
}
