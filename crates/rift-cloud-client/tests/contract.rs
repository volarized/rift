//! Contract checks for the published global package API.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use rift_cloud_client::contract::{self, ContractError};
use rift_cloud_client::{
    Capabilities, PackageDeclarationResponse, PackagePatternPage, PackageResolutionResponse,
    PackageSearchCandidate, PackageSearchPage, PackageSymbolCandidate, PackageSymbolPage,
};
use serde_json::{Map, Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn repository_root() -> TestResult<PathBuf> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .ok_or("the crate sits two directories below the repository root")?
        .to_path_buf())
}

fn contract() -> TestResult<Value> {
    Ok(serde_json::from_slice(&fs::read(
        repository_root()?.join(contract::CONTRACT_PATH),
    )?)?)
}

fn write_contract(document: &Value) -> TestResult<(tempfile::TempDir, PathBuf)> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("global-api.openapi.json");
    fs::write(&path, serde_json::to_vec_pretty(document)?)?;
    Ok((directory, path))
}

fn assert_invalid(document: &Value, rule: &str) -> TestResult {
    let (_directory, path) = write_contract(document)?;
    let error = contract::validate(&path).expect_err("changed contract must fail");
    assert!(error.source().is_none());
    let ContractError::Invalid {
        rule: actual_rule, ..
    } = &error
    else {
        panic!("expected Invalid, got {error:?}");
    };
    assert!(actual_rule.contains(rule), "{actual_rule}");
    assert!(error.to_string().contains("fails contract validation"));
    Ok(())
}

#[test]
fn committed_contract_is_valid() -> TestResult {
    contract::validate(&repository_root()?.join(contract::CONTRACT_PATH))?;
    Ok(())
}

/// Every schema `pattern` compiles in the two dialects that read it: the ECMA-262 regular
/// expressions a JSON Schema validator runs, and the `regex` crate the client generator
/// compiles, which has no lookahead. A lookahead the validator accepts fails the contract.
#[test]
fn every_schema_pattern_compiles_for_the_validator_and_the_generator() -> TestResult {
    let document = contract()?;
    let mut pending = vec![&document];
    let mut patterns = Vec::new();
    while let Some(value) = pending.pop() {
        match value {
            Value::Object(members) => {
                if let Some(Value::String(pattern)) = members.get("pattern") {
                    patterns.push(pattern.clone());
                }
                pending.extend(
                    members
                        .iter()
                        .filter(|(member, _)| member.as_str() != "examples")
                        .map(|(_, value)| value),
                );
            }
            Value::Array(items) => pending.extend(items),
            _ => {}
        }
    }
    let project_path = &document["components"]["schemas"]["ProjectPath"]["pattern"];
    assert!(
        patterns.iter().any(|pattern| project_path == pattern),
        "the project path pattern is among them: {patterns:#?}"
    );
    for pattern in &patterns {
        jsonschema::validator_for(&json!({ "type": "string", "pattern": pattern }))
            .map_err(|error| format!("{pattern}: {error}"))?;
        regex::Regex::new(pattern).map_err(|error| format!("{pattern}: {error}"))?;
    }

    let mut document = contract()?;
    document["components"]["schemas"]["ProjectPath"]["pattern"] = json!(r"^(?!\.rift)[a-z]*$");
    assert_invalid(&document, "must compile under the `regex` crate")?;
    Ok(())
}

#[test]
fn missing_and_malformed_contracts_keep_their_sources() -> TestResult {
    let directory = tempfile::tempdir()?;
    let missing = directory.path().join("missing.json");
    let error = contract::validate(&missing).expect_err("missing contract must fail");
    assert!(matches!(error, ContractError::Read { .. }));
    assert!(error.source().is_some());
    assert!(error.to_string().starts_with("cannot read `"));

    let malformed = directory.path().join("malformed.json");
    fs::write(&malformed, "{")?;
    let error = contract::validate(&malformed).expect_err("malformed contract must fail");
    assert!(matches!(error, ContractError::Parse { .. }));
    assert!(error.source().is_some());
    assert!(error.to_string().starts_with("cannot parse `"));
    Ok(())
}

#[test]
fn fixed_surface_changes_fail_validation() -> TestResult {
    let mut document = contract()?;
    document["openapi"] = json!("3.0.4");
    assert_invalid(&document, "OpenAPI version")?;

    let mut document = contract()?;
    document["paths"]
        .as_object_mut()
        .ok_or("paths must be an object")?
        .remove("/v1/symbols");
    assert_invalid(&document, "paths differ")?;

    let mut document = contract()?;
    document["paths"]["/v1/search"]["post"]["operationId"] = json!("search");
    assert_invalid(&document, "operationId")?;

    let mut document = contract()?;
    document["paths"]["/v1/search"]["post"]["security"] = json!([]);
    assert_invalid(&document, "optional bearer authentication")?;

    let mut document = contract()?;
    document["servers"] = json!([{"url": "https://example.com/rift/rest"}]);
    assert_invalid(&document, "`servers` must stay absent")?;
    Ok(())
}

#[test]
fn request_and_response_changes_fail_validation() -> TestResult {
    let mut document = contract()?;
    document["paths"]["/v1/capabilities"]["get"]["requestBody"] =
        document["paths"]["/v1/search"]["post"]["requestBody"].clone();
    assert_invalid(&document, "must not declare a request body")?;

    let mut document = contract()?;
    document["paths"]["/v1/resolutions"]["post"]["requestBody"]["required"] = json!(false);
    assert_invalid(&document, "request body must be required")?;

    let mut document = contract()?;
    document["paths"]["/v1/search"]["post"]["responses"]
        .as_object_mut()
        .ok_or("responses must be an object")?
        .remove("504");
    assert_invalid(&document, "response statuses differ")?;

    let mut document = contract()?;
    document["paths"]["/v1/search"]["post"]["responses"]["400"]["content"] =
        json!({"application/json": {}});
    assert_invalid(&document, "application/problem+json")?;

    let mut document = contract()?;
    document["paths"]["/v1/capabilities"]["get"]["responses"]["304"]["content"] =
        json!({"application/json": {}});
    assert_invalid(&document, "304 response must not carry content")?;

    let mut document = contract()?;
    document["paths"]["/v1/resolutions"]["post"]["requestBody"]["content"] =
        json!({"application/problem+json": {}});
    assert_invalid(
        &document,
        "request body must declare only `application/json`",
    )?;
    Ok(())
}

#[test]
fn bound_and_shared_schema_changes_fail_validation() -> TestResult {
    let mut document = contract()?;
    document["components"]["parameters"]["Limit"]["schema"]["maximum"] = json!(201);
    assert_invalid(&document, "Limit/schema/maximum")?;

    let mut document = contract()?;
    document["components"]["parameters"]["Limit"]["schema"]["default"] = json!(101);
    assert_invalid(&document, "Limit/schema/default")?;

    let mut document = contract()?;
    document["components"]["schemas"]["CapabilityBounds"]["properties"]["page_limit_max"]["maximum"] =
        json!(200);
    assert_invalid(
        &document,
        "CapabilityBounds/properties/page_limit_max/maximum",
    )?;

    let mut document = contract()?;
    document["components"]["schemas"]["PackageSymbolPage"]["properties"]["items"]["maxItems"] =
        json!(200);
    assert_invalid(&document, "PackageSymbolPage/properties/items/maxItems")?;

    let mut document = contract()?;
    document["components"]["parameters"]["Cursor"]["schema"]["maxLength"] = json!(4095);
    assert_invalid(&document, "Cursor/schema/maxLength")?;

    let mut document = contract()?;
    document["components"]["schemas"]["PackagePatternRequest"]["properties"]["pattern"]["maxLength"] =
        json!(4096);
    assert_invalid(
        &document,
        "PackagePatternRequest/properties/pattern/maxLength",
    )?;

    let mut document = contract()?;
    document["paths"]["/v1/patterns"]["post"]["x-rift-page-files-max"] = json!(2000);
    assert_invalid(&document, "x-rift-page-files-max")?;

    let mut document = contract()?;
    document["components"]["schemas"]["PackageDeclarationRequest"]["properties"]["positions"]["maxItems"] =
        json!(10000);
    assert_invalid(
        &document,
        "PackageDeclarationRequest/properties/positions/maxItems",
    )?;

    let mut document = contract()?;
    document["components"]["schemas"]["PackagePosition"]["properties"]["character"]["maximum"] =
        json!(65535);
    assert_invalid(&document, "PackagePosition/properties/character/maximum")?;

    let mut document = contract()?;
    document["components"]["schemas"]["PackageIdentity"]["properties"]["name"]["maxLength"] =
        json!(4095);
    assert_invalid(&document, "PackageIdentity")?;

    let mut document = contract()?;
    document["components"]["schemas"]["Warning"]["properties"]["detail"]["maxLength"] = json!(1024);
    assert_invalid(&document, "Warning/properties/detail/maxLength")?;

    // A longer requirement makes a longer `requirement_unsatisfied` detail than the warning
    // bound holds.
    let mut document = contract()?;
    document["components"]["schemas"]["PackageContextEntry"]["properties"]["requirement"]["maxLength"] =
        json!(8192);
    assert_invalid(&document, "Warning/properties/detail/maxLength")?;
    Ok(())
}

/// An optional property accepting `null` fails validation, in a shared schema and in one the
/// contract alone defines: the service omits an absent field.
#[test]
fn an_optional_property_accepting_null_fails_validation() -> TestResult {
    let mut document = contract()?;
    document["components"]["schemas"]["PackageSearchPage"]["properties"]["next_cursor"]["type"] =
        json!(["string", "null"]);
    assert_invalid(&document, "schema `PackageSearchPage` accepts `null`")?;

    let mut document = contract()?;
    document["components"]["schemas"]["Symbol"]["properties"]["visibility"]["type"] =
        json!(["string", "null"]);
    assert_invalid(&document, "schema `Symbol` accepts `null`")?;
    Ok(())
}

#[test]
fn every_contract_example_validates_against_its_schema() -> TestResult {
    let document = contract()?;
    let components = document["components"]["schemas"]
        .as_object()
        .ok_or("components.schemas must be an object")?;
    let mut checked = 0_usize;
    validate_examples(&document, components, &mut checked)?;
    assert_eq!(
        checked, 15,
        "each request and success response carries an example"
    );
    Ok(())
}

#[test]
fn every_contract_response_example_decodes_through_generated_types() -> TestResult {
    let document = contract()?;
    let mut checked = 0_usize;
    decode_examples(&document, &mut checked)?;
    assert_eq!(
        checked, 8,
        "each success response carries a generated response type"
    );
    Ok(())
}

fn decode_examples(node: &Value, checked: &mut usize) -> TestResult {
    match node {
        Value::Object(object) => {
            if let (Some(reference), Some(examples)) = (
                object
                    .get("schema")
                    .and_then(|schema| schema.get("$ref"))
                    .and_then(Value::as_str),
                object.get("examples").and_then(Value::as_object),
            ) {
                for example in examples.values() {
                    let value = example
                        .get("value")
                        .ok_or("a contract example must carry a value")?;
                    if decode_generated_response_example(reference, value.clone())? {
                        *checked += 1;
                    }
                }
            }
            for value in object.values() {
                decode_examples(value, checked)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                decode_examples(value, checked)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn decode_generated_response_example(reference: &str, value: Value) -> TestResult<bool> {
    match reference {
        "#/components/schemas/Capabilities" => {
            serde_json::from_value::<Capabilities>(value)?;
        }
        "#/components/schemas/PackageResolutionResponse" => {
            serde_json::from_value::<PackageResolutionResponse>(value)?;
        }
        "#/components/schemas/PackageSearchPage" => {
            let page = serde_json::from_value::<PackageSearchPage>(value)?;
            for item in page.items {
                PackageSearchCandidate::try_from(item)?;
            }
        }
        "#/components/schemas/PackageSymbolPage" => {
            let page = serde_json::from_value::<PackageSymbolPage>(value)?;
            for item in page.items {
                PackageSymbolCandidate::try_from(item)?;
            }
        }
        "#/components/schemas/PackagePatternPage" => {
            serde_json::from_value::<PackagePatternPage>(value)?;
        }
        "#/components/schemas/PackageDeclarationResponse" => {
            serde_json::from_value::<PackageDeclarationResponse>(value)?;
        }
        "#/components/schemas/PackageResolutionRequest"
        | "#/components/schemas/PackageSearchRequest"
        | "#/components/schemas/PackageSymbolRequest"
        | "#/components/schemas/PackagePatternRequest"
        | "#/components/schemas/PackageDeclarationRequest" => return Ok(false),
        other => return Err(format!("contract example names no generated type: {other}").into()),
    }
    Ok(true)
}

fn validate_examples(
    node: &Value,
    components: &Map<String, Value>,
    checked: &mut usize,
) -> TestResult {
    match node {
        Value::Object(object) => {
            if let (Some(schema), Some(examples)) = (
                object.get("schema"),
                object.get("examples").and_then(Value::as_object),
            ) {
                let schema = standalone_schema(schema, components);
                let validator = jsonschema::validator_for(&schema)?;
                for example in examples.values() {
                    let value = example
                        .get("value")
                        .ok_or("a contract example must carry a value")?;
                    let failures = validator
                        .iter_errors(value)
                        .map(|error| error.to_string())
                        .collect::<Vec<_>>();
                    assert!(failures.is_empty(), "example failures: {failures:#?}");
                    *checked += 1;
                }
            }
            for value in object.values() {
                validate_examples(value, components, checked)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_examples(value, components, checked)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn standalone_schema(schema: &Value, components: &Map<String, Value>) -> Value {
    let mut schema = schema.clone();
    rewrite_references(&mut schema);
    let mut definitions = Value::Object(components.clone());
    rewrite_references(&mut definitions);
    let object = schema
        .as_object_mut()
        .expect("a media type schema must be an object");
    object.insert(
        "$schema".to_owned(),
        json!("https://json-schema.org/draft/2020-12/schema"),
    );
    object.insert("$defs".to_owned(), definitions);
    schema
}

fn rewrite_references(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                let rewritten = reference.replace("#/components/schemas/", "#/$defs/");
                object.insert("$ref".to_owned(), Value::String(rewritten));
            }
            for child in object.values_mut() {
                rewrite_references(child);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(rewrite_references),
        _ => {}
    }
}
