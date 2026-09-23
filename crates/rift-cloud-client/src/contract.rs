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
use rift_protocol::read::{PackageIdentity, SourceUnitId, Symbol, SymbolId};
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

const ENDPOINTS: [Endpoint; 4] = [
    Endpoint::Capabilities,
    Endpoint::Resolutions,
    Endpoint::Search,
    Endpoint::Symbols,
];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Endpoint {
    Capabilities,
    Resolutions,
    Search,
    Symbols,
}

impl Endpoint {
    pub(crate) const fn path(self) -> &'static str {
        match self {
            Self::Capabilities => "/v1/capabilities",
            Self::Resolutions => "/v1/resolutions",
            Self::Search => "/v1/search",
            Self::Symbols => "/v1/symbols",
        }
    }

    pub(crate) fn method(self) -> Method {
        match self {
            Self::Capabilities => Method::GET,
            Self::Resolutions | Self::Search | Self::Symbols => Method::POST,
        }
    }

    pub(crate) const fn operation_id(self) -> &'static str {
        match self {
            Self::Capabilities => "getCapabilities",
            Self::Resolutions => "resolvePackageContext",
            Self::Search => "searchPackages",
            Self::Symbols => "listPackageSymbols",
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
    let expected_max_items = [
        (
            "PackageResolutionRequest",
            "entries",
            DEPENDENCIES_PACKAGES_MAX,
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

    let expected_max_lengths = [
        ("PackageSearchRequest", "query", QUERY_BYTES_MAX),
        ("QueryTerm", "text", QUERY_TERM_BYTES_MAX),
    ];
    for (component, property, value) in expected_max_lengths {
        let schema = property_schema(spec, component, property)?;
        expect_bound(
            &format!("{component}/properties/{property}/maxLength"),
            schema.max_length,
            u64::try_from(value).map_err(|error| error.to_string())?,
        )?;
    }

    let limit = parameter_schema(spec, "Limit")?;
    expect_bound("Limit/schema/default", limit.default, json!(100))?;
    expect_bound(
        "Limit/schema/maximum",
        limit.maximum,
        serde_json::Number::from(200),
    )?;
    let cursor = parameter_schema(spec, "Cursor")?;
    expect_bound("Cursor/schema/maxLength", cursor.max_length, 4096)?;
    Ok(())
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

    // oas3 0.22 omits JSON Schema keywords used by shared models, including
    // patternProperties. Compare those schemas as JSON to retain their exact shape.
    let components = document
        .get("components")
        .and_then(|components| components.get("schemas"))
        .and_then(Value::as_object)
        .ok_or_else(|| "components/schemas must be an object".to_owned())?;
    for (name, mut expected) in generator.take_definitions(true) {
        normalize_shared_string_enum(&mut expected);
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
