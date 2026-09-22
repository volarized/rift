//! Validation for the published global package API contract.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use schemars::generate::SchemaSettings;
use serde_json::{Map, Value, json};

use rift_protocol::dependencies::{DEPENDENCIES_PACKAGES_MAX, PackageContextEntry};
use rift_protocol::read::{PackageIdentity, SourceUnitId, Symbol, SymbolId};
use rift_ranking::{
    IDENTIFIER_CANDIDATES_MAX, PARSED_QUERY_MEMBERS_MAX, QUERY_BYTES_MAX, QUERY_TERM_BYTES_MAX,
};

/// Published path below the repository root.
pub const CONTRACT_PATH: &str = "docs/public/global-api.openapi.json";
/// Published path below the docs directory.
pub const DOCUMENT_PATH: &str = "public/global-api.openapi.json";

const SERVER_URL: &str = "https://api.volar.sh/rift/rest";
const ERROR_STATUSES: [&str; 10] = [
    "400", "401", "403", "406", "413", "415", "429", "500", "502", "503",
];

const OPERATIONS: [(&str, &str, &str); 4] = [
    ("/v1/capabilities", "get", "getCapabilities"),
    ("/v1/resolutions", "post", "resolvePackageContext"),
    ("/v1/search", "post", "searchPackages"),
    ("/v1/symbols", "post", "listPackageSymbols"),
];

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
    expect_value(&document, "/openapi", &json!("3.1.0")).map_err(&invalid)?;
    expect_value(
        &document,
        "/jsonSchemaDialect",
        &json!("https://json-schema.org/draft/2020-12/schema"),
    )
    .map_err(&invalid)?;
    expect_value(&document, "/servers/0/url", &json!(SERVER_URL)).map_err(&invalid)?;
    expect_value(&document, "/security", &optional_bearer()).map_err(&invalid)?;
    expect_value(
        &document,
        "/components/securitySchemes/bearerAuth/type",
        &json!("http"),
    )
    .map_err(&invalid)?;
    expect_value(
        &document,
        "/components/securitySchemes/bearerAuth/scheme",
        &json!("bearer"),
    )
    .map_err(&invalid)?;

    validate_operations(&document).map_err(&invalid)?;
    validate_bounds(&document).map_err(&invalid)?;
    validate_shared_schemas(&document).map_err(&invalid)?;
    Ok(())
}

fn validate_operations(document: &Value) -> Result<(), String> {
    let paths = object_at(document, "/paths")?;
    let actual_paths = paths.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected_paths = OPERATIONS
        .iter()
        .map(|(path, _, _)| *path)
        .collect::<BTreeSet<_>>();
    if actual_paths != expected_paths {
        return Err(format!(
            "paths differ: expected {expected_paths:?}, found {actual_paths:?}"
        ));
    }

    for (path, method, operation_id) in OPERATIONS {
        let pointer = format!("/paths/{}/{method}", escape_pointer(path));
        let operation = object_at(document, &pointer)?;
        if operation.get("operationId") != Some(&json!(operation_id)) {
            return Err(format!(
                "{method} {path} must use operationId `{operation_id}`"
            ));
        }
        if operation.get("security") != Some(&optional_bearer()) {
            return Err(format!(
                "{method} {path} must declare optional bearer authentication"
            ));
        }

        let responses = operation
            .get("responses")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("{method} {path} must declare responses"))?;
        let mut expected = ERROR_STATUSES.into_iter().collect::<BTreeSet<_>>();
        expected.insert("200");
        expected.insert("504");
        if method == "get" {
            expected.remove("413");
            expected.remove("415");
            expected.insert("304");
        }
        let actual = responses
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if actual != expected {
            return Err(format!(
                "{method} {path} response statuses differ: expected {expected:?}, found {actual:?}"
            ));
        }
        validate_response_media(path, method, responses)?;
        validate_request(path, method, operation)?;
    }
    Ok(())
}

fn validate_response_media(
    path: &str,
    method: &str,
    responses: &Map<String, Value>,
) -> Result<(), String> {
    for (status, response) in responses {
        let content = response.get("content");
        if status == "304" {
            if content.is_some() {
                return Err(format!(
                    "{method} {path} 304 response must not carry content"
                ));
            }
            continue;
        }
        let content = content
            .and_then(Value::as_object)
            .ok_or_else(|| format!("{method} {path} {status} response must declare content"))?;
        let expected = if status == "200" {
            "application/json"
        } else {
            "application/problem+json"
        };
        if content.len() != 1 || !content.contains_key(expected) {
            return Err(format!(
                "{method} {path} {status} response must declare only `{expected}`"
            ));
        }
    }
    Ok(())
}

fn validate_request(
    path: &str,
    method: &str,
    operation: &Map<String, Value>,
) -> Result<(), String> {
    if method == "get" {
        if operation.contains_key("requestBody") {
            return Err(format!("{method} {path} must not declare a request body"));
        }
        return Ok(());
    }
    let request = operation
        .get("requestBody")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{method} {path} must declare a request body"))?;
    if request.get("required") != Some(&json!(true)) {
        return Err(format!("{method} {path} request body must be required"));
    }
    let content = request
        .get("content")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{method} {path} request body must declare content"))?;
    if content.len() != 1 || !content.contains_key("application/json") {
        return Err(format!(
            "{method} {path} request body must declare only `application/json`"
        ));
    }
    Ok(())
}

fn validate_bounds(document: &Value) -> Result<(), String> {
    let expected = [
        (
            "/components/schemas/PackageResolutionRequest/properties/entries/maxItems",
            DEPENDENCIES_PACKAGES_MAX,
        ),
        (
            "/components/schemas/PackageSearchRequest/properties/query/maxLength",
            QUERY_BYTES_MAX,
        ),
        (
            "/components/schemas/PackageSearchRequest/properties/terms/maxItems",
            PARSED_QUERY_MEMBERS_MAX,
        ),
        (
            "/components/schemas/QueryTerm/properties/text/maxLength",
            QUERY_TERM_BYTES_MAX,
        ),
        (
            "/components/schemas/PackageSearchRequest/properties/identifiers/maxItems",
            IDENTIFIER_CANDIDATES_MAX,
        ),
    ];
    for (pointer, value) in expected {
        expect_value(document, pointer, &json!(value))?;
    }
    expect_value(
        document,
        "/components/parameters/Limit/schema/default",
        &json!(100),
    )?;
    expect_value(
        document,
        "/components/parameters/Limit/schema/maximum",
        &json!(200),
    )?;
    expect_value(
        document,
        "/components/parameters/Cursor/schema/maxLength",
        &json!(4096),
    )?;
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

    let components = object_at(document, "/components/schemas")?;
    for (name, expected) in generator.take_definitions(true) {
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

fn expect_value(document: &Value, pointer: &str, expected: &Value) -> Result<(), String> {
    let actual = document
        .pointer(pointer)
        .ok_or_else(|| format!("`{pointer}` is missing"))?;
    if actual != expected {
        return Err(format!(
            "`{pointer}` differs: expected {expected}, found {actual}"
        ));
    }
    Ok(())
}

fn object_at<'document>(
    document: &'document Value,
    pointer: &str,
) -> Result<&'document Map<String, Value>, String> {
    document
        .pointer(pointer)
        .and_then(Value::as_object)
        .ok_or_else(|| format!("`{pointer}` must be an object"))
}

fn escape_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn optional_bearer() -> Value {
    json!([{}, {"bearerAuth": []}])
}
