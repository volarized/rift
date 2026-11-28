//! Schemas specific to the global package API.

use rift_protocol::{
    configuration::{GLOBAL_REQUEST_BYTES_MAX, GLOBAL_RESPONSE_BYTES_MAX, GLOBAL_SOURCE_BYTES_MAX},
    dependencies::DEPENDENCIES_PACKAGES_MAX,
    documentation::DOCUMENTATION_EXCERPT_BYTES_MAX,
    read::SEARCH_PATTERN_CHARS_MAX,
};
use rift_ranking::{
    IDENTIFIER_CANDIDATES_MAX, PARSED_QUERY_MEMBERS_MAX, QUERY_BYTES_MAX, QUERY_TERM_BYTES_MAX,
};
use serde_json::{Map, Value, json};

pub(super) fn schemas() -> Map<String, Value> {
    let mut schemas = super::shared_schemas();
    schemas.extend(
        publication()
            .into_iter()
            .chain(resolution())
            .chain(search())
            .chain(patterns())
            .chain(headers())
            .chain(selectors())
            .map(|(name, schema)| (name.to_owned(), schema)),
    );
    schemas
}

#[expect(
    clippy::too_many_lines,
    reason = "the schemas list the wire fields and bounds"
)]
fn publication() -> [(&'static str, Value); 4] {
    [
        (
            "PublicationFormat",
            json!({
                "description": "Publication format used by the package index.",
                "type": "string",
                "enum": ["rift-package-index-v2", "unknown"],
            }),
        ),
        (
            "Warning",
            json!({
                "additionalProperties": true,
                "description": "One condition the caller must account for.",
                "properties": {
                    "code": {
                        "description": "Stable warning code.",
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                        "enum": [
                            "query_narrowed",
                            "source_truncated",
                            "publication_changed",
                            "capability_unavailable",
                            "result_truncated",
                            "requirement_unsatisfied",
                            "unknown",
                        ],
                    },
                    "detail": {
                        "description": "Bounded explanation for the warning. The bound holds the longest `requirement_unsatisfied` detail the package fields admit: a 128-byte manager, and a 4,096-byte name, requirement, and version.",
                        "type": "string",
                        "minLength": 1,
                        "maxLength": crate::WARNING_DETAIL_CHARS_MAX,
                    },
                },
                "required": ["code"],
                "type": "object",
            }),
        ),
        (
            "CapabilityBounds",
            json!({
                "additionalProperties": true,
                "description": "Active server ceilings for requests and responses.",
                "properties": {
                    "request_body_bytes_max": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": GLOBAL_REQUEST_BYTES_MAX,
                    },
                    "response_body_bytes_max": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": GLOBAL_RESPONSE_BYTES_MAX,
                    },
                    "dependency_entries_max": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20000,
                    },
                    "query_bytes_max": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 4096,
                    },
                    "query_terms_max": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 32,
                    },
                    "query_term_bytes_max": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 256,
                    },
                    "identifiers_max": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 16,
                    },
                    "identifier_bytes_max": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 4096,
                    },
                    "packages_max": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20000,
                    },
                    "page_limit_min": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": crate::PAGE_LIMIT_MAX,
                    },
                    "page_limit_max": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": crate::PAGE_LIMIT_MAX,
                    },
                    "page_limit_default": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": crate::PAGE_LIMIT_MAX,
                    },
                    "candidate_pool_max": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000,
                    },
                    "warnings_max": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 32,
                    },
                    "source_bytes_max": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": GLOBAL_SOURCE_BYTES_MAX,
                    },
                },
                "required": [
                    "request_body_bytes_max",
                    "response_body_bytes_max",
                    "dependency_entries_max",
                    "query_bytes_max",
                    "query_terms_max",
                    "query_term_bytes_max",
                    "identifiers_max",
                    "identifier_bytes_max",
                    "packages_max",
                    "page_limit_min",
                    "page_limit_max",
                    "page_limit_default",
                    "candidate_pool_max",
                    "warnings_max",
                    "source_bytes_max",
                ],
                "type": "object",
            }),
        ),
        (
            "Capabilities",
            json!({
                "additionalProperties": true,
                "description": "Capabilities and active bounds for the package API.",
                "properties": {
                    "supported_package_managers": {
                        "description": "Supported package managers.",
                        "items": {
                            "description": "Package manager name.",
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 128,
                        },
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 3,
                    },
                    "supported_features": {
                        "description": "Supported API features.",
                        "items": {
                            "description": "Feature name.",
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 128,
                        },
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 32,
                    },
                    "publication_format": { "$ref": "#/components/schemas/PublicationFormat" },
                    "analyzer_revision": {
                        "description": "Analyzer revision used to produce declarations.",
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                    },
                    "corpus_revision": {
                        "description": "Corpus revision used to answer reads.",
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                    },
                    "required_search_fields": {
                        "description": "Fields required for package search.",
                        "items": {
                            "description": "Search field name.",
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 128,
                        },
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 32,
                    },
                    "bounds": { "$ref": "#/components/schemas/CapabilityBounds" },
                    "documentation_revision": {
                        "type": "string",
                        "minLength": 8,
                        "maxLength": 8,
                        "pattern": "^[0-9a-f]{8}$",
                        "description": "Documentation revision used for documentation reads.",
                    },
                    "edge_capabilities": {
                        "description": "Complete edge operations supported by existing collected facts. Absence establishes no supported operation.",
                        "type": "array",
                        "maxItems": 2,
                        "items": { "$ref": "#/components/schemas/EdgeCapability" },
                    },
                },
                "required": [
                    "supported_package_managers",
                    "supported_features",
                    "publication_format",
                    "analyzer_revision",
                    "corpus_revision",
                    "required_search_fields",
                    "bounds",
                ],
                "type": "object",
            }),
        ),
    ]
}

fn resolution() -> [(&'static str, Value); 3] {
    [
        (
            "PackageResolutionRequest",
            json!({
                "additionalProperties": false,
                "description": "Canonical dependency context submitted for package resolution.",
                "properties": {
                    "entries": {
                        "description": "Canonical dependency context entries.",
                        "items": { "$ref": "#/components/schemas/PackageContextEntry" },
                        "type": "array",
                        "maxItems": DEPENDENCIES_PACKAGES_MAX,
                    },
                },
                "required": ["entries"],
                "type": "object",
            }),
        ),
        (
            "ResolvedRequirement",
            json!({
                "additionalProperties": true,
                "description": "One submitted entry paired with the package that answers it: a requirement with the release it resolves to, or an exact version with the nearest collected release.",
                "properties": {
                    "entry": { "$ref": "#/components/schemas/PackageContextEntry" },
                    "package": { "$ref": "#/components/schemas/PackageIdentity" },
                },
                "required": ["entry", "package"],
                "type": "object",
            }),
        ),
        (
            "PackageResolutionResponse",
            json!({
                "additionalProperties": true,
                "description": "Resolution result accounting for each submitted entry.",
                "properties": {
                    "available_exact": {
                        "description": "Exact packages available in the global index.",
                        "items": { "$ref": "#/components/schemas/PackageIdentity" },
                        "type": "array",
                        "maxItems": 20000,
                    },
                    "resolved_requirements": {
                        "description": "Requirements resolved to exact packages, and exact versions the global index answers from the nearest collected release.",
                        "items": { "$ref": "#/components/schemas/ResolvedRequirement" },
                        "type": "array",
                        "maxItems": 20000,
                    },
                    "missing_exact": {
                        "description": "Exact packages absent from the global index.",
                        "items": { "$ref": "#/components/schemas/PackageIdentity" },
                        "type": "array",
                        "maxItems": 20000,
                    },
                    "missing_requirements": {
                        "description": "Requirements absent from the global index.",
                        "items": { "$ref": "#/components/schemas/PackageContextEntry" },
                        "type": "array",
                        "maxItems": 20000,
                    },
                    "warnings": {
                        "description": "Conditions the caller must account for, absent when none applies. A `requirement_unsatisfied` warning names a requirement of `resolved_requirements` whose package lies outside the range the requirement states, since no collected release satisfies it; its `detail` reads `<manager>/<name> <requirement> answered by <version>`, as in `npm/typescript ~5.7.2 answered by 5.9.3`.",
                        "items": { "$ref": "#/components/schemas/Warning" },
                        "type": "array",
                        "maxItems": crate::DEPENDENCY_ENTRIES_MAX,
                    },
                },
                "required": [
                    "available_exact",
                    "resolved_requirements",
                    "missing_exact",
                    "missing_requirements",
                ],
                "type": "object",
            }),
        ),
    ]
}

#[expect(
    clippy::too_many_lines,
    reason = "the schemas list the wire fields and bounds"
)]
fn search() -> [(&'static str, Value); 8] {
    [
        (
            "QueryTerm",
            json!({
                "additionalProperties": false,
                "description": "One bounded member from the parsed query.",
                "properties": {
                    "text": {
                        "description": "One parsed query term or phrase.",
                        "type": "string",
                        "minLength": 1,
                        "maxLength": QUERY_TERM_BYTES_MAX,
                    },
                    "phrase": {
                        "description": "Whether the caller quoted this term.",
                        "type": "boolean",
                    },
                    "prefix": {
                        "description": "Whether the remote index widens this term as a prefix.",
                        "type": "boolean",
                    },
                },
                "required": ["text", "phrase", "prefix"],
                "type": "object",
            }),
        ),
        (
            "SearchScope",
            json!({
                "anyOf": [
                    {
                        "enum": ["local", "global", "all"],
                    },
                    {
                        "not": {
                            "pattern": "[@:](?:local|global|all)(?::|$)",
                        },
                        "pattern": "^local(?:@[a-z][a-z0-9_-]*)?(?::[a-z][a-z0-9_-]*)?$",
                    },
                ],
                "description": "Which corpus a read searches. local@name selects an accepted extra project; local:name selects a root-defined profile; local@name:profile selects a profile defined by that extra project.",
                "maxLength": 8192,
                "type": "string",
            }),
        ),
        (
            "PackageSearchRequest",
            json!({
                "additionalProperties": false,
                "description": "A lexical package declaration search or a bounded edge walk.",
                "properties": {
                    "query": {
                        "description": "Original query text.",
                        "type": "string",
                        "minLength": 1,
                        "maxLength": QUERY_BYTES_MAX,
                    },
                    "terms": {
                        "description": "Ordered members of `query`, as Rift parses it. The server refuses a request whose members differ from the ones it parses from `query`.",
                        "items": { "$ref": "#/components/schemas/QueryTerm" },
                        "type": "array",
                        "minItems": 1,
                        "maxItems": PARSED_QUERY_MEMBERS_MAX,
                    },
                    "identifiers": {
                        "description": "Identifier candidates of `query`, in the order Rift extracts them. The server refuses a request whose list differs from the candidates it extracts from `query`.",
                        "items": {
                            "description": "Extracted identifier.",
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 4096,
                        },
                        "type": "array",
                        "maxItems": IDENTIFIER_CANDIDATES_MAX,
                    },
                    "include": {
                        "$ref": "#/components/schemas/EdgeInclude",
                        "description": "Optional fields to include. An omitted member is `false`.",
                    },
                    "packages": {
                        "description": "Selected exact package versions. Omitted or empty, use the newest collected stable version of each package, or its newest prerelease when no stable version is collected.",
                        "items": { "$ref": "#/components/schemas/PackageIdentity" },
                        "type": "array",
                        "maxItems": 20000,
                    },
                    "phase": {
                        "description": "Ranking phase to execute.",
                        "type": "string",
                        "enum": ["precise", "broad"],
                    },
                    "target": {
                        "description": "Which package results may be returned. Omitted, symbol.",
                        "type": "string",
                        "enum": ["symbol", "documentation", "all"],
                    },
                    "traversal": {
                        "description": "A bounded edge walk from an explicit canonical seed.",
                        "$ref": "#/components/schemas/SearchTraversal",
                    },
                    "view": {
                        "description": "A view returned by a preceding read. Expiry never selects latest implicitly.",
                        "$ref": "#/components/schemas/CapturedViewId",
                    },
                    "revision": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                        "description": "The indexed revision to read. Omitted reads the served revision.",
                    },
                },
                "type": "object",
                "oneOf": [
                    {
                        "required": ["query", "terms", "identifiers", "phase"],
                    },
                    {
                        "required": ["traversal"],
                        "not": {
                            "anyOf": [
                                {
                                    "required": ["query"],
                                },
                                {
                                    "required": ["terms"],
                                },
                                {
                                    "required": ["identifiers"],
                                },
                                {
                                    "required": ["phase"],
                                },
                            ],
                        },
                    },
                ],
                "allOf": [
                    {
                        "properties": {
                            "traversal": {
                                "required": ["seed"],
                            },
                        },
                    },
                    {
                        "if": {
                            "required": ["traversal"],
                        },
                        "then": {
                            "properties": {
                                "target": {
                                    "const": "symbol",
                                },
                            },
                        },
                    },
                ],
            }),
        ),
        (
            "IdentifierMatchClass",
            json!({
                "description": "Match class established for one declaration identifier. A search hit that matches none of the requested identifiers reports `unknown`; a symbol lookup hit never does.",
                "type": "string",
                "enum": ["qualified_exact", "name_exact", "name_prefix", "substring", "unknown"],
            }),
        ),
        (
            "PackageSearchHit",
            json!({
                "additionalProperties": true,
                "description": "One public package declaration returned by search.",
                "properties": {
                    "package": { "$ref": "#/components/schemas/PackageIdentity" },
                    "symbol": { "$ref": "#/components/schemas/Symbol" },
                    "qualified_name": {
                        "description": "The provider's qualified declaration name used for identifier ranking, with its original spelling.",
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 8192,
                    },
                    "unit": { "$ref": "#/components/schemas/SourceUnitId" },
                    "range": { "$ref": "#/components/schemas/TextRange" },
                    "line": {
                        "description": "One-based source line containing the declaration.",
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 9_007_199_254_740_991_u64,
                    },
                    "contributing_fields": {
                        "description": "Fields that contributed to ranking. `file_content` names a match inside the declaration, which the file's text holds.",
                        "items": {
                            "type": "string",
                            "enum": [
                                "name",
                                "qualified_name",
                                "documentation",
                                "signature",
                                "file_content",
                                "unknown",
                            ],
                        },
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 16,
                    },
                    "match_class": { "$ref": "#/components/schemas/IdentifierMatchClass" },
                    "source": {
                        "description": "Optional declaration source excerpt.",
                        "type": "string",
                        "maxLength": GLOBAL_SOURCE_BYTES_MAX,
                    },
                },
                "required": [
                    "package",
                    "symbol",
                    "qualified_name",
                    "unit",
                    "range",
                    "line",
                    "contributing_fields",
                    "match_class",
                ],
                "type": "object",
            }),
        ),
        (
            "PackageSearchPage",
            json!({
                "additionalProperties": true,
                "description": "One page of package declarations. The response carries pagination for the selected declarations. A page above response_body_bytes_max is refused.",
                "properties": {
                    "items": {
                        "description": "Ranked package declarations.",
                        "items": { "$ref": "#/components/schemas/PackageSearchItem" },
                        "type": "array",
                        "maxItems": crate::PAGE_LIMIT_MAX,
                    },
                    "warnings": {
                        "description": "Warnings attached to this page.",
                        "items": { "$ref": "#/components/schemas/Warning" },
                        "type": "array",
                        "maxItems": 32,
                    },
                    "publication_format": { "$ref": "#/components/schemas/PublicationFormat" },
                    "analyzer_revision": {
                        "description": "Analyzer revision used for this page.",
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                    },
                    "corpus_revision": {
                        "description": "Corpus revision used for this page.",
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                    },
                    "documentation_revision": {
                        "type": "string",
                        "minLength": 8,
                        "maxLength": 8,
                        "pattern": "^[0-9a-f]{8}$",
                        "description": "Documentation revision used for this page.",
                    },
                    "view": { "$ref": "#/components/schemas/CapturedView" },
                    "pagination": { "$ref": "#/components/schemas/Pagination" },
                    "revision": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                        "description": "The indexed revision this answer read.",
                    },
                },
                "required": [
                    "items",
                    "warnings",
                    "publication_format",
                    "analyzer_revision",
                    "corpus_revision",
                    "view",
                    "pagination",
                    "revision",
                ],
                "type": "object",
            }),
        ),
        (
            "PackageDocumentationHit",
            json!({
                "additionalProperties": true,
                "description": "One documentation block returned by search.",
                "properties": {
                    "target": {
                        "const": "documentation",
                        "type": "string",
                    },
                    "package": { "$ref": "#/components/schemas/PackageIdentity" },
                    "documentation": { "$ref": "#/components/schemas/DocumentationHit" },
                    "contributing_fields": {
                        "description": "Fields that contributed to ranking.",
                        "items": {
                            "type": "string",
                            "enum": ["documentation", "content", "unknown"],
                        },
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 16,
                    },
                    "source": {
                        "description": "The block source excerpt when requested, bounded across the response page.",
                        "type": "string",
                        "maxLength": DOCUMENTATION_EXCERPT_BYTES_MAX,
                    },
                },
                "required": ["target", "package", "documentation", "contributing_fields"],
                "type": "object",
            }),
        ),
        (
            "PackageSearchItem",
            json!({
                "description": "One symbol or documentation result.",
                "oneOf": [
                    {
                        "allOf": [
                            { "$ref": "#/components/schemas/PackageSearchHit" },
                            {
                                "not": {
                                    "required": ["hit"],
                                },
                            },
                        ],
                    },
                    {
                        "allOf": [
                            { "$ref": "#/components/schemas/PackageDocumentationHit" },
                            {
                                "not": {
                                    "required": ["hit"],
                                },
                            },
                        ],
                    },
                    {
                        "allOf": [
                            { "$ref": "#/components/schemas/SearchHit" },
                            {
                                "properties": {
                                    "hit": {
                                        "properties": {
                                            "target": {
                                                "const": "symbol",
                                            },
                                        },
                                    },
                                },
                            },
                        ],
                    },
                ],
            }),
        ),
    ]
}

#[expect(
    clippy::too_many_lines,
    reason = "the schemas list the wire fields and bounds"
)]
fn patterns() -> [(&'static str, Value); 4] {
    [
        (
            "PackagePatternRequest",
            json!({
                "additionalProperties": false,
                "description": "Regular expression search over the source of selected exact package versions.",
                "properties": {
                    "pattern": {
                        "description": "Regular expression in the syntax of the Rust `regex` crate. The server matches it line by line: `^` and `$` match at line boundaries, and no character class matches a line feed. The inline flag `(?i)` makes it case-insensitive.",
                        "type": "string",
                        "minLength": 1,
                        "maxLength": SEARCH_PATTERN_CHARS_MAX,
                    },
                    "packages": {
                        "description": "Selected exact package versions. Omitted or empty, use the newest collected stable version of each package, or its newest prerelease when no stable version is collected.",
                        "items": { "$ref": "#/components/schemas/PackageIdentity" },
                        "type": "array",
                        "maxItems": 20000,
                    },
                    "include": {
                        "description": "Optional fields to include. An omitted member is `false`.",
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "source": {
                                "description": "Attaches the line where each match begins and the source of its enclosing declaration.",
                                "type": "boolean",
                            },
                        },
                    },
                    "revision": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                        "description": "The indexed revision to read. Omitted reads the served revision.",
                    },
                },
                "required": ["pattern"],
                "type": "object",
            }),
        ),
        (
            "PackagePatternDeclaration",
            json!({
                "additionalProperties": true,
                "description": "The smallest declaration whose range holds a match. A match no declaration holds carries none.",
                "properties": {
                    "symbol": { "$ref": "#/components/schemas/Symbol" },
                    "range": { "$ref": "#/components/schemas/TextRange" },
                    "line": {
                        "description": "One-based source line containing the declaration.",
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 9_007_199_254_740_991_u64,
                    },
                    "source": {
                        "description": "The declaration's source, when `include.source` is `true`.",
                        "type": "string",
                        "maxLength": GLOBAL_SOURCE_BYTES_MAX,
                    },
                },
                "required": ["symbol", "range", "line"],
                "type": "object",
            }),
        ),
        (
            "PackagePatternHit",
            json!({
                "additionalProperties": true,
                "description": "One match of the pattern in one package file.",
                "properties": {
                    "package": { "$ref": "#/components/schemas/PackageIdentity" },
                    "unit": { "$ref": "#/components/schemas/SourceUnitId" },
                    "range": { "$ref": "#/components/schemas/TextRange" },
                    "line": {
                        "description": "One-based source line where the match begins.",
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 9_007_199_254_740_991_u64,
                    },
                    "size": {
                        "description": "Size in bytes of the file holding the match.",
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 9_007_199_254_740_991_u64,
                    },
                    "declaration": { "$ref": "#/components/schemas/PackagePatternDeclaration" },
                    "source": {
                        "description": "The line where the match begins, without its line ending, when `include.source` is `true`.",
                        "type": "string",
                        "maxLength": GLOBAL_SOURCE_BYTES_MAX,
                    },
                },
                "required": ["package", "unit", "range", "line", "size"],
                "type": "object",
            }),
        ),
        (
            "PackagePatternPage",
            json!({
                "additionalProperties": true,
                "description": "One page of results at the named revision. A page past the last returns an empty list with the same pagination. The server refuses a page that exceeds the response byte bound.",
                "properties": {
                    "items": {
                        "description": "Matches in package, file path, and offset order.",
                        "items": { "$ref": "#/components/schemas/PackagePatternHit" },
                        "type": "array",
                        "maxItems": crate::PAGE_LIMIT_MAX,
                    },
                    "warnings": {
                        "description": "Warnings attached to this page.",
                        "items": { "$ref": "#/components/schemas/Warning" },
                        "type": "array",
                        "maxItems": 32,
                    },
                    "publication_format": { "$ref": "#/components/schemas/PublicationFormat" },
                    "analyzer_revision": {
                        "description": "Analyzer revision used for this page.",
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                    },
                    "corpus_revision": {
                        "description": "Corpus revision used for this page.",
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                    },
                    "view": {
                        "$ref": "#/components/schemas/CapturedView",
                        "description": "The original immutable serving view and expiry. Older responses can omit it; reads requiring captured source context return unavailable.",
                    },
                    "pagination": { "$ref": "#/components/schemas/Pagination" },
                    "revision": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 128,
                        "description": "The indexed revision this answer read.",
                    },
                },
                "required": [
                    "items",
                    "warnings",
                    "publication_format",
                    "analyzer_revision",
                    "corpus_revision",
                    "pagination",
                    "revision",
                ],
                "type": "object",
            }),
        ),
    ]
}

fn headers() -> [(&'static str, Value); 6] {
    [
        (
            "ProblemDetails",
            json!({
                "additionalProperties": true,
                "description": "RFC 9457 problem details for an application error.",
                "properties": {
                    "type": {
                        "type": "string",
                        "format": "uri",
                        "minLength": 1,
                        "maxLength": 2048,
                    },
                    "title": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 256,
                    },
                    "status": {
                        "type": "integer",
                        "minimum": 400,
                        "maximum": 599,
                    },
                    "detail": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 4096,
                    },
                    "instance": {
                        "type": "string",
                        "format": "uri",
                        "maxLength": 2048,
                    },
                },
                "required": ["type", "title", "status", "detail"],
                "type": "object",
            }),
        ),
        (
            "RetryAfter",
            json!({
                "description": "Bounded retry delay in seconds.",
                "type": "integer",
                "minimum": 0,
                "maximum": 86400,
            }),
        ),
        (
            "RateLimitLimit",
            json!({
                "description": "Bounded request limit for the current period.",
                "type": "integer",
                "minimum": 1,
                "maximum": 1_000_000_000,
            }),
        ),
        (
            "RateLimitRemaining",
            json!({
                "description": "Bounded requests remaining in the current period.",
                "type": "integer",
                "minimum": 0,
                "maximum": 1_000_000_000,
            }),
        ),
        (
            "RateLimitReset",
            json!({
                "description": "Unix seconds until the current rate limit resets.",
                "type": "integer",
                "minimum": 0,
                "maximum": 2_147_483_647,
            }),
        ),
        (
            "WWWAuthenticate",
            json!({
                "description": "Bearer authentication challenge.",
                "type": "string",
                "minLength": 1,
                "maxLength": 256,
            }),
        ),
    ]
}

#[expect(
    clippy::too_many_lines,
    reason = "the schemas list the wire fields and bounds"
)]
fn selectors() -> [(&'static str, Value); 9] {
    [
        (
            "SearchParamsTarget",
            json!({
                "description": "Which entity kinds may be returned.",
                "type": "string",
                "enum": ["symbol", "file", "documentation", "all", "commit"],
                "x-enum-descriptions": [
                    "Only declarations may match.",
                    "Only tree entries may match.",
                    "Only collected documentation blocks may match.",
                    "Any declaration, tree entry, or documentation block may match.",
                    "Only commits the history store holds may match, by their message.",
                ],
            }),
        ),
        (
            "ResultOrder",
            json!({
                "description": "The total order a paginated answer comes back in. Every order ends in the result's own\nidentity, so two results that tie never swap places between pages.",
                "type": "string",
                "enum": ["relevance", "path", "identity"],
                "x-enum-descriptions": [
                    "Exact original identifier spelling first, then best score, with identity breaking ties.",
                    "Project path order, with identity breaking ties.",
                    "The result's own identity alone.",
                ],
            }),
        ),
        (
            "RequestedPackage",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "manager": {
                        "type": "string",
                        "description": "Package manager or ecosystem name, as `cargo`, `npm`, or `pypi`.",
                        "minLength": 1,
                        "maxLength": 128,
                    },
                    "name": {
                        "type": "string",
                        "description": "Package name in that ecosystem.",
                        "minLength": 1,
                        "maxLength": 4096,
                    },
                    "registry": {
                        "type": "string",
                        "description": "Accepted defining registry endpoint. Absent when the registry is unresolved.",
                        "minLength": 1,
                        "maxLength": 4096,
                    },
                    "version": {
                        "type": "string",
                        "description": "The exact version to read. Absent, the entry asks for the requirement `>=0`, which\nthe global index answers from its newest collected release.",
                        "minLength": 1,
                        "maxLength": 4096,
                    },
                },
                "required": ["manager", "name"],
                "description": "One package a `get_symbol` or `search` request names beside the dependency context.",
                "examples": [
                    {
                        "manager": "cargo",
                        "registry": "crates.io",
                        "name": "tokio",
                        "version": "1.47.1",
                    },
                    {
                        "manager": "npm",
                        "registry": "npmjs.org",
                        "name": "zod",
                    },
                ],
            }),
        ),
        (
            "PathSelector",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "include": {
                        "type": "array",
                        "items": { "$ref": "#/components/schemas/PathPattern" },
                        "description": "Globs a path has to match to be searched at all. Empty includes every visible file.",
                        "default": [],
                    },
                    "exclude": {
                        "type": "array",
                        "items": { "$ref": "#/components/schemas/PathPattern" },
                        "description": "Globs that drop a path `include` already matched.",
                        "default": [],
                    },
                    "force_include": {
                        "type": "array",
                        "items": { "$ref": "#/components/schemas/PathPattern" },
                        "description": "Globs reaching files the workspace's `[source]` policy or `.gitignore` excluded from\nthe index. Matches are bounded per request - the bound counts the files the request\nreaches outside the index, so a glob whose matches the index already holds adds none -\nand the server refuses the search when the bound is crossed rather than truncating it\nsilently.",
                        "default": [],
                    },
                },
                "description": "Which files a query runs over, as three lists of globs matched against the project-relative\npath. The same glob engine backs the workspace's `[source]` policy. `include: [\"src/**\"]`\nselects the source tree; `exclude: [\"src/generated/**\"]` then removes generated output.",
                "examples": [
                    {
                        "include": ["src/**"],
                        "exclude": ["src/generated/**"],
                    },
                ],
            }),
        ),
        (
            "PathPattern",
            json!({
                "type": "string",
                "examples": ["src/**"],
                "pattern": r"^(?:\.*[^\\\u0000-\u001F\u007F/.][^\\\u0000-\u001F\u007F/]*|\.{3,})(?:/(?:\.*[^\\\u0000-\u001F\u007F/.][^\\\u0000-\u001F\u007F/]*|\.{3,})?)*$",
                "description": "Project-relative glob using *, ?, **, and character classes. Forward-slash separated on\nevery platform, whatever separator the host OS uses natively.",
            }),
        ),
        (
            "SearchInclude",
            json!({
                "description": "Extra payload to attach to every hit. An omitted member is `false`. Every selected member\ncosts response bytes per hit, so the caller requests only what it will read.",
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "source": {
                        "type": "boolean",
                        "description": "Attaches the source text around each hit.",
                    },
                    "score": {
                        "type": "boolean",
                        "description": "Attaches the ranking value of each hit. Exact original identifier spelling ranks before\nthis value.",
                    },
                },
                "examples": [
                    {
                        "source": true,
                    },
                ],
            }),
        ),
        (
            "SearchChange",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "base": {
                        "$ref": "#/components/schemas/RevisionId",
                        "description": "The revision compared from - a branch, tag, or commit id as the workspace's version\ncontrol spells it.",
                    },
                    "head": {
                        "$ref": "#/components/schemas/ChangeHead",
                        "description": "The side compared to: a revision spelled the same way, or the working tree.\nOmitted, `HEAD`.",
                        "default": "HEAD",
                    },
                },
                "required": ["base"],
                "description": "Two sides to compare. The answer is the declarations the two sides hold differently:\n`base` is the committed revision compared from, `head` the revision or the working tree\ncompared to.",
                "examples": [
                    {
                        "base": "main",
                        "head": "HEAD",
                    },
                    {
                        "base": "HEAD",
                        "head": {
                            "kind": "working_tree",
                        },
                    },
                ],
            }),
        ),
        (
            "ChangeHead",
            json!({
                "anyOf": [
                    {
                        "$ref": "#/components/schemas/RevisionId",
                        "description": "A branch, tag, or commit id as the workspace's version control spells it.",
                    },
                    {
                        "$ref": "#/components/schemas/ChangeTree",
                        "description": "The working tree as the current index serves it: staged and unstaged edits, files\ngit does not track, and deleted files.",
                    },
                ],
                "description": "The side a comparison compares to: a committed revision, or the working tree with its\nuncommitted edits.",
            }),
        ),
        (
            "ChangeTree",
            json!({
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "kind": {
                                "type": "string",
                                "const": "working_tree",
                            },
                        },
                        "required": ["kind"],
                        "additionalProperties": false,
                        "description": "The working tree, uncommitted edits included.",
                    },
                ],
                "description": "A comparison side that names a tree rather than a revision, spelled\n`{\"kind\": \"working_tree\"}`: no revision spelling can collide with it.",
            }),
        ),
    ]
}
