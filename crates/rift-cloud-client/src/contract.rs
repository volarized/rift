//! Validation for the published global package contract.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use http::{Method, StatusCode};
use oas3::Spec;
use oas3::spec::{ObjectOrReference, ObjectSchema, Operation, Response, Schema, SecurityScheme};
use schemars::generate::SchemaSettings;
use serde_json::{Value, json};

use rift_protocol::dependencies::{DEPENDENCIES_PACKAGES_MAX, PackageContextEntry};
use rift_protocol::documentation::{DocumentationContext, DocumentationHit};
use rift_protocol::read::{
    PackageIdentity, SEARCH_PATTERN_CHARS_MAX, SourceUnitId, Symbol, SymbolId,
};
use rift_ranking::{
    IDENTIFIER_CANDIDATES_MAX, PARSED_QUERY_MEMBERS_MAX, QUERY_BYTES_MAX, QUERY_TERM_BYTES_MAX,
};

/// Published path below the repository root.
pub const CONTRACT_PATH: &str = "docs/public/global-api.openapi.json";
/// Published path below the docs directory.
pub const DOCUMENT_PATH: &str = "public/global-api.openapi.json";

const ERROR_STATUSES: [StatusCode; 10] = [
    StatusCode::BAD_REQUEST,
    StatusCode::UNAUTHORIZED,
    StatusCode::FORBIDDEN,
    StatusCode::NOT_ACCEPTABLE,
    StatusCode::PAYLOAD_TOO_LARGE,
    StatusCode::UNSUPPORTED_MEDIA_TYPE,
    StatusCode::TOO_MANY_REQUESTS,
    StatusCode::INTERNAL_SERVER_ERROR,
    StatusCode::BAD_GATEWAY,
    StatusCode::SERVICE_UNAVAILABLE,
];

const ENDPOINTS: [Endpoint; 6] = [
    Endpoint::Capabilities,
    Endpoint::Resolutions,
    Endpoint::Search,
    Endpoint::Symbols,
    Endpoint::Patterns,
    Endpoint::Declarations,
];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Endpoint {
    Capabilities,
    Resolutions,
    Search,
    Symbols,
    Patterns,
    Declarations,
}

impl Endpoint {
    pub(crate) const fn path(self) -> &'static str {
        match self {
            Self::Capabilities => "/v1/capabilities",
            Self::Resolutions => "/v1/resolutions",
            Self::Search => "/v1/search",
            Self::Symbols => "/v1/symbols",
            Self::Patterns => "/v1/patterns",
            Self::Declarations => "/v1/declarations",
        }
    }

    pub(crate) fn method(self) -> Method {
        match self {
            Self::Capabilities => Method::GET,
            Self::Resolutions
            | Self::Search
            | Self::Symbols
            | Self::Patterns
            | Self::Declarations => Method::POST,
        }
    }

    pub(crate) const fn operation_id(self) -> &'static str {
        match self {
            Self::Capabilities => "getCapabilities",
            Self::Resolutions => "resolvePackageContext",
            Self::Search => "searchPackages",
            Self::Symbols => "listPackageSymbols",
            Self::Patterns => "searchPackagePatterns",
            Self::Declarations => "findPackageDeclarations",
        }
    }

    fn statuses(self) -> BTreeSet<StatusCode> {
        let mut statuses = ERROR_STATUSES.into_iter().collect::<BTreeSet<_>>();
        statuses.insert(StatusCode::OK);
        statuses.insert(StatusCode::GATEWAY_TIMEOUT);
        if self == Self::Capabilities {
            statuses.remove(&StatusCode::PAYLOAD_TOO_LARGE);
            statuses.remove(&StatusCode::UNSUPPORTED_MEDIA_TYPE);
            statuses.insert(StatusCode::NOT_MODIFIED);
        }
        statuses
    }
}

/// Why the published global API contract could not be validated.
#[derive(Debug)]
pub enum ContractError {
    /// The artifact could not be read.
    Read {
        /// Artifact path.
        path: PathBuf,
        /// Read failure.
        source: io::Error,
    },
    /// The artifact is not JSON.
    Parse {
        /// Artifact path.
        path: PathBuf,
        /// JSON failure.
        source: serde_json::Error,
    },
    /// One contract rule does not hold.
    Invalid {
        /// Artifact path.
        path: PathBuf,
        /// Failed rule.
        rule: String,
    },
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "cannot read `{}`: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(formatter, "cannot parse `{}`: {source}", path.display())
            }
            Self::Invalid { path, rule } => {
                write!(
                    formatter,
                    "`{}` fails contract validation: {rule}",
                    path.display()
                )
            }
        }
    }
}

impl Error for ContractError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Invalid { .. } => None,
        }
    }
}

/// Validates the published global API artifact against its fixed operations and shared Rust
/// schemas.
///
/// # Errors
///
/// Returns [`ContractError`] when the artifact cannot be read or parsed, or one required rule does
/// not hold.
pub fn validate(path: &Path) -> Result<(), ContractError> {
    let bytes = fs::read(path).map_err(|source| ContractError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let document: Value =
        serde_json::from_slice(&bytes).map_err(|source| ContractError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    let invalid = |rule: String| ContractError::Invalid {
        path: path.to_path_buf(),
        rule,
    };
    let spec: Spec = serde_json::from_value(document.clone())
        .map_err(|error| invalid(format!("OpenAPI document cannot be decoded: {error}")))?;
    if spec.openapi != "3.1.0" {
        return Err(invalid(format!(
            "OpenAPI version must be `3.1.0`, found `{}`",
            spec.openapi
        )));
    }
    if document.get("jsonSchemaDialect").and_then(Value::as_str)
        != Some("https://json-schema.org/draft/2020-12/schema")
    {
        return Err(invalid(
            "JSON Schema dialect must be draft 2020-12".to_owned(),
        ));
    }
    if document.get("servers").is_some() {
        return Err(invalid(
            "root `servers` must stay absent; client configuration selects the endpoint".to_owned(),
        ));
    }
    validate_security(&spec).map_err(&invalid)?;
    validate_operations(&spec).map_err(&invalid)?;
    validate_bounds(&spec).map_err(&invalid)?;
    validate_optional_properties_omit_null(&document).map_err(&invalid)?;
    validate_patterns(&document).map_err(&invalid)?;
    validate_shared_schemas(&document).map_err(&invalid)?;
    Ok(())
}

fn validate_security(spec: &Spec) -> Result<(), String> {
    if !is_optional_bearer(&spec.security) {
        return Err("root security must declare optional bearer authentication".to_owned());
    }
    let scheme = spec
        .components
        .as_ref()
        .and_then(|components| components.security_schemes.get("bearerAuth"))
        .ok_or_else(|| "bearerAuth security scheme is missing".to_owned())?
        .resolve(spec)
        .map_err(|error| format!("bearerAuth security scheme cannot resolve: {error}"))?;
    match scheme {
        SecurityScheme::Http { scheme, .. } if scheme == "bearer" => Ok(()),
        _ => Err("bearerAuth must be an HTTP bearer scheme".to_owned()),
    }
}

fn validate_operations(spec: &Spec) -> Result<(), String> {
    let paths = spec
        .paths
        .as_ref()
        .ok_or_else(|| "paths are missing".to_owned())?;
    let actual_paths = paths.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected_paths = ENDPOINTS
        .iter()
        .map(|endpoint| endpoint.path())
        .collect::<BTreeSet<_>>();
    if actual_paths != expected_paths {
        return Err(format!(
            "paths differ: expected {expected_paths:?}, found {actual_paths:?}"
        ));
    }

    let actual_operations = spec
        .operations()
        .map(|(path, method, _)| (path, method))
        .collect::<HashSet<_>>();
    let expected_operations = ENDPOINTS
        .iter()
        .map(|endpoint| (endpoint.path().to_owned(), endpoint.method()))
        .collect::<HashSet<_>>();
    if actual_operations != expected_operations {
        return Err(format!(
            "operations differ: expected {expected_operations:?}, found {actual_operations:?}"
        ));
    }

    for endpoint in ENDPOINTS {
        let path = endpoint.path();
        let method = endpoint.method();
        let operation = spec
            .operation(&method, path)
            .ok_or_else(|| format!("{method} {path} is missing"))?;
        if operation.operation_id.as_deref() != Some(endpoint.operation_id()) {
            return Err(format!(
                "{method} {path} must use operationId `{}`",
                endpoint.operation_id()
            ));
        }
        if !is_optional_bearer(&operation.security) {
            return Err(format!(
                "{method} {path} must declare optional bearer authentication"
            ));
        }

        let responses = resolved_responses(spec, operation)?;
        let expected = endpoint.statuses();
        let actual = responses.keys().copied().collect::<BTreeSet<_>>();
        if actual != expected {
            return Err(format!(
                "{method} {path} response statuses differ: expected {expected:?}, found {actual:?}"
            ));
        }
        validate_response_media(endpoint, &responses)?;
        validate_request(spec, endpoint, operation)?;
    }
    Ok(())
}

fn resolved_responses(
    spec: &Spec,
    operation: &Operation,
) -> Result<BTreeMap<StatusCode, Response>, String> {
    let responses = operation
        .responses
        .as_ref()
        .ok_or_else(|| "operation must declare responses".to_owned())?;
    responses
        .iter()
        .map(|(wire_status, response)| {
            let status = wire_status
                .parse::<StatusCode>()
                .map_err(|_| format!("unsupported response status `{wire_status}`"))?;
            let response = response
                .resolve(spec)
                .map_err(|error| format!("response {wire_status} cannot resolve: {error}"))?;
            Ok((status, response))
        })
        .collect()
}

fn validate_response_media(
    endpoint: Endpoint,
    responses: &BTreeMap<StatusCode, Response>,
) -> Result<(), String> {
    let path = endpoint.path();
    let method = endpoint.method();
    for (status, response) in responses {
        if *status == StatusCode::NOT_MODIFIED {
            if !response.content.is_empty() {
                return Err(format!(
                    "{method} {path} 304 response must not carry content"
                ));
            }
            continue;
        }
        let expected = if *status == StatusCode::OK {
            "application/json"
        } else {
            "application/problem+json"
        };
        if response.content.len() != 1 || !response.content.contains_key(expected) {
            return Err(format!(
                "{method} {path} {status} response must declare only `{expected}`"
            ));
        }
    }
    Ok(())
}

fn validate_request(spec: &Spec, endpoint: Endpoint, operation: &Operation) -> Result<(), String> {
    let path = endpoint.path();
    let method = endpoint.method();
    if method == Method::GET {
        if operation.request_body.is_some() {
            return Err(format!("{method} {path} must not declare a request body"));
        }
        return Ok(());
    }
    let request = operation
        .request_body(spec)
        .map_err(|error| format!("{method} {path} request body cannot resolve: {error}"))?
        .ok_or_else(|| format!("{method} {path} must declare a request body"))?;
    if request.required != Some(true) {
        return Err(format!("{method} {path} request body must be required"));
    }
    if request.content.len() != 1 || !request.content.contains_key("application/json") {
        return Err(format!(
            "{method} {path} request body must declare only `application/json`"
        ));
    }
    Ok(())
}

fn validate_bounds(spec: &Spec) -> Result<(), String> {
    // Before the pins below, so a package field raised past the detail bound names the
    // detail it no longer fits.
    validate_warning_detail_bound(spec)?;
    let page_items_max =
        usize::try_from(crate::PAGE_LIMIT_MAX).map_err(|error| error.to_string())?;
    let expected_max_items = [
        (
            "PackageResolutionRequest",
            "entries",
            DEPENDENCIES_PACKAGES_MAX,
        ),
        (
            "PackageResolutionResponse",
            "warnings",
            crate::DEPENDENCY_ENTRIES_MAX,
        ),
        ("PackageSearchPage", "items", page_items_max),
        ("PackageSymbolPage", "items", page_items_max),
        ("PackagePatternPage", "items", page_items_max),
        (
            "PackageDeclarationRequest",
            "positions",
            crate::DECLARATION_POSITIONS_MAX,
        ),
        (
            "PackageDeclarationResponse",
            "results",
            crate::DECLARATION_POSITIONS_MAX,
        ),
        ("PackageSearchRequest", "terms", PARSED_QUERY_MEMBERS_MAX),
        (
            "PackageSearchRequest",
            "identifiers",
            IDENTIFIER_CANDIDATES_MAX,
        ),
    ];
    for (component, property, value) in expected_max_items {
        let schema = property_schema(spec, component, property)?;
        expect_bound(
            &format!("{component}/properties/{property}/maxItems"),
            schema.max_items,
            u64::try_from(value).map_err(|error| error.to_string())?,
        )?;
    }
    validate_max_lengths(spec)?;

    let limit = parameter_schema(spec, "Limit")?;
    expect_bound("Limit/schema/default", limit.default, json!(100))?;
    expect_bound(
        "Limit/schema/maximum",
        limit.maximum,
        serde_json::Number::from(crate::PAGE_LIMIT_MAX),
    )?;
    for property in ["page_limit_min", "page_limit_max", "page_limit_default"] {
        let schema = property_schema(spec, "CapabilityBounds", property)?;
        expect_bound(
            &format!("CapabilityBounds/properties/{property}/maximum"),
            schema.maximum,
            serde_json::Number::from(crate::PAGE_LIMIT_MAX),
        )?;
    }
    let cursor = parameter_schema(spec, "Cursor")?;
    expect_bound("Cursor/schema/maxLength", cursor.max_length, 4096)?;
    for property in ["line", "character"] {
        let schema = property_schema(spec, "PackagePosition", property)?;
        expect_bound(
            &format!("PackagePosition/properties/{property}/maximum"),
            schema.maximum,
            serde_json::Number::from(crate::POSITION_COMPONENT_MAX),
        )?;
    }
    validate_pattern_page_bound(spec)
}

/// Pins each string bound the client enforces to the contract's `maxLength` for it.
fn validate_max_lengths(spec: &Spec) -> Result<(), String> {
    let expected_max_lengths = [
        ("PackageSearchRequest", "query", QUERY_BYTES_MAX),
        ("QueryTerm", "text", QUERY_TERM_BYTES_MAX),
        ("PackagePatternRequest", "pattern", SEARCH_PATTERN_CHARS_MAX),
        ("Warning", "detail", crate::WARNING_DETAIL_CHARS_MAX),
        (
            "PackageIdentity",
            "manager",
            crate::PACKAGE_MANAGER_CHARS_MAX,
        ),
        ("PackageIdentity", "name", crate::PACKAGE_NAME_CHARS_MAX),
        (
            "PackageIdentity",
            "version",
            crate::PACKAGE_VERSION_CHARS_MAX,
        ),
        (
            "PackageContextEntry",
            "manager",
            crate::PACKAGE_MANAGER_CHARS_MAX,
        ),
        ("PackageContextEntry", "name", crate::PACKAGE_NAME_CHARS_MAX),
        (
            "PackageContextEntry",
            "version",
            crate::PACKAGE_VERSION_CHARS_MAX,
        ),
        (
            "PackageContextEntry",
            "requirement",
            crate::PACKAGE_VERSION_CHARS_MAX,
        ),
    ];
    for (component, property, value) in expected_max_lengths {
        let schema = property_schema(spec, component, property)?;
        expect_bound(
            &format!("{component}/properties/{property}/maxLength"),
            schema.max_length,
            u64::try_from(value).map_err(|error| error.to_string())?,
        )?;
    }
    Ok(())
}

/// Pins the warning `detail` bound to the longest `requirement_unsatisfied` detail the
/// contract's own package fields admit: `<manager>/<name> <requirement> answered by
/// <version>`, each part at its `maxLength`.
fn validate_warning_detail_bound(spec: &Spec) -> Result<(), String> {
    let parts = [
        ("PackageContextEntry", "manager"),
        ("PackageContextEntry", "name"),
        ("PackageContextEntry", "requirement"),
        ("PackageIdentity", "version"),
    ];
    let mut longest = u64::try_from("/".len() + " ".len() + crate::ANSWERED_BY.len())
        .map_err(|error| error.to_string())?;
    for (component, property) in parts {
        let bound = property_schema(spec, component, property)?
            .max_length
            .ok_or_else(|| format!("{component}/properties/{property}/maxLength is missing"))?;
        longest = longest.saturating_add(bound);
    }
    let detail = property_schema(spec, "Warning", "detail")?;
    expect_bound(
        "Warning/properties/detail/maxLength",
        detail.max_length,
        longest,
    )
}

/// Pins the files bound the pattern operation states to the one the client enforces.
fn validate_pattern_page_bound(spec: &Spec) -> Result<(), String> {
    let endpoint = Endpoint::Patterns;
    let operation = spec
        .operation(&endpoint.method(), endpoint.path())
        .ok_or_else(|| format!("{} is missing", endpoint.path()))?;
    let expected =
        u64::try_from(crate::PATTERN_PAGE_FILES_MAX).map_err(|error| error.to_string())?;
    expect_bound(
        "x-rift-page-files-max",
        operation
            .extensions
            .get("rift-page-files-max")
            .and_then(Value::as_u64),
        expected,
    )
}

fn property_schema(spec: &Spec, component: &str, property: &str) -> Result<ObjectSchema, String> {
    let component_schema = component_schema(spec, component)?;
    let schema = component_schema
        .properties
        .get(property)
        .ok_or_else(|| format!("{component}/properties/{property} is missing"))?;
    resolve_object_schema(spec, schema, &format!("{component}/properties/{property}"))
}

fn component_schema(spec: &Spec, name: &str) -> Result<ObjectSchema, String> {
    let schema = spec
        .components
        .as_ref()
        .and_then(|components| components.schemas.get(name))
        .ok_or_else(|| format!("component schema `{name}` is missing"))?;
    resolve_object_schema(spec, schema, name)
}

fn parameter_schema(spec: &Spec, name: &str) -> Result<ObjectSchema, String> {
    let parameter = spec
        .components
        .as_ref()
        .and_then(|components| components.parameters.get(name))
        .ok_or_else(|| format!("parameter `{name}` is missing"))?
        .resolve(spec)
        .map_err(|error| format!("parameter `{name}` cannot resolve: {error}"))?;
    let schema = parameter
        .schema
        .as_ref()
        .ok_or_else(|| format!("{name}/schema is missing"))?;
    resolve_object_schema(spec, schema, &format!("{name}/schema"))
}

fn resolve_object_schema(spec: &Spec, schema: &Schema, name: &str) -> Result<ObjectSchema, String> {
    match schema
        .resolve(spec)
        .map_err(|error| format!("schema `{name}` cannot resolve: {error}"))?
    {
        Schema::Object(schema) => match *schema {
            ObjectOrReference::Object(schema) => Ok(schema),
            ObjectOrReference::Ref { .. } => {
                Err(format!("schema `{name}` did not resolve to an object"))
            }
        },
        Schema::Boolean(_) => Err(format!("schema `{name}` must be an object")),
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "oas3 yields owned bound values and the mismatch reports both values"
)]
fn expect_bound<T>(name: &str, actual: Option<T>, expected: T) -> Result<(), String>
where
    T: fmt::Debug + PartialEq,
{
    if actual.as_ref() != Some(&expected) {
        return Err(format!(
            "{name} differs: expected {expected:?}, found {actual:?}"
        ));
    }
    Ok(())
}

/// The component schemas of `document`.
fn component_schemas(document: &Value) -> Result<&serde_json::Map<String, Value>, String> {
    document
        .get("components")
        .and_then(|components| components.get("schemas"))
        .and_then(Value::as_object)
        .ok_or_else(|| "components/schemas must be an object".to_owned())
}

/// Refuses a component schema whose optional property accepts `null`.
///
/// The service omits an absent optional field, as the MCP surface does, so an optional
/// property's `null` arm advertises a value no answer carries. The rule is the one
/// `rift_protocol::schema::strip_optional_null_arms` applies to the served MCP schemas: a
/// schema that stripping changes still holds such an arm.
fn validate_optional_properties_omit_null(document: &Value) -> Result<(), String> {
    for (name, schema) in component_schemas(document)? {
        let Some(schema) = schema.as_object() else {
            continue;
        };
        let mut stripped = schema.clone();
        rift_protocol::schema::strip_optional_null_arms(&mut stripped);
        if &stripped != schema {
            return Err(format!(
                "schema `{name}` accepts `null` on an optional property; the service omits an \
                 absent field, so the property states its value type alone"
            ));
        }
    }
    Ok(())
}

/// Refuses a schema `pattern` the `regex` crate cannot compile.
///
/// A JSON Schema validator reads a pattern as an ECMA-262 regular expression, and the client
/// generator compiles it with the `regex` crate, which has no lookaround: a pattern outside
/// the dialect both read leaves the generated client without the check, and the generator
/// only prints a warning. The walk skips `examples`, whose values are data.
fn validate_patterns(document: &Value) -> Result<(), String> {
    let mut pending = vec![("", document)];
    while let Some((key, value)) = pending.pop() {
        match value {
            Value::String(pattern) if key == "pattern" => {
                regex::Regex::new(pattern).map_err(|error| {
                    format!(
                        "pattern `{pattern}` must compile under the `regex` crate the client \
                         generator reads it with: {error}"
                    )
                })?;
            }
            Value::Object(members) => pending.extend(
                members
                    .iter()
                    .filter(|(member, _)| !matches!(member.as_str(), "examples" | "example"))
                    .map(|(member, value)| (member.as_str(), value)),
            ),
            Value::Array(items) => pending.extend(items.iter().map(|item| ("", item))),
            _ => {}
        }
    }
    Ok(())
}

/// Compares each shared schema with its Rust model's, in the form the MCP surface serves:
/// with the `null` arm stripped from every optional property.
fn validate_shared_schemas(document: &Value) -> Result<(), String> {
    let mut settings = SchemaSettings::draft2020_12();
    settings.definitions_path = "/components/schemas".into();
    settings.meta_schema = None;
    let mut generator = settings.into_generator();
    let _ = generator.subschema_for::<PackageContextEntry>();
    let _ = generator.subschema_for::<PackageIdentity>();
    let _ = generator.subschema_for::<SourceUnitId>();
    let _ = generator.subschema_for::<Symbol>();
    let _ = generator.subschema_for::<SymbolId>();
    let _ = generator.subschema_for::<DocumentationHit>();
    let _ = generator.subschema_for::<DocumentationContext>();

    // oas3 0.22 omits JSON Schema keywords used by shared models, including
    // patternProperties. Compare those schemas as JSON to retain their exact shape.
    let components = component_schemas(document)?;
    for (name, mut expected) in generator.take_definitions(true) {
        normalize_shared_string_enum(&mut expected);
        if let Some(schema) = expected.as_object_mut() {
            rift_protocol::schema::strip_optional_null_arms(schema);
        }
        let actual = components
            .get(&name)
            .ok_or_else(|| format!("shared schema `{name}` is missing"))?;
        if actual != &expected {
            return Err(format!(
                "shared schema `{name}` differs from its Rust model"
            ));
        }
    }
    Ok(())
}

fn normalize_shared_string_enum(schema: &mut Value) {
    let Some(object) = schema.as_object_mut() else {
        return;
    };
    let Some(variants) = object.get("oneOf").and_then(Value::as_array) else {
        return;
    };
    let values = variants
        .iter()
        .map(|variant| variant.get("const").and_then(Value::as_str))
        .collect::<Option<Vec<_>>>();
    let descriptions = variants
        .iter()
        .map(|variant| variant.get("description").and_then(Value::as_str))
        .collect::<Option<Vec<_>>>();
    let (Some(values), Some(descriptions)) = (values, descriptions) else {
        return;
    };
    let values = values.into_iter().map(str::to_owned).collect::<Vec<_>>();
    let descriptions = descriptions
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    object.remove("oneOf");
    object.insert("type".to_owned(), json!("string"));
    object.insert("enum".to_owned(), json!(values));
    object.insert("x-enum-descriptions".to_owned(), json!(descriptions));
}

fn is_optional_bearer(requirements: &[oas3::spec::SecurityRequirement]) -> bool {
    requirements.len() == 2
        && requirements
            .iter()
            .any(|requirement| requirement.0.is_empty())
        && requirements.iter().any(|requirement| {
            requirement.0.len() == 1 && requirement.0.get("bearerAuth").is_some_and(Vec::is_empty)
        })
}
