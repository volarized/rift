//! Contract checks for the published global package API.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use rift_mcp::global_api::{self, ContractError};
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
        repository_root()?.join(global_api::CONTRACT_PATH),
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
    let error = global_api::validate(&path).expect_err("changed contract must fail");
    let ContractError::Invalid {
        rule: actual_rule, ..
    } = error
    else {
        panic!("expected Invalid, got {error:?}");
    };
    assert!(actual_rule.contains(rule), "{actual_rule}");
    Ok(())
}

#[test]
fn committed_contract_is_valid() -> TestResult {
    global_api::validate(&repository_root()?.join(global_api::CONTRACT_PATH))?;
    Ok(())
}

#[test]
fn missing_and_malformed_contracts_keep_their_sources() -> TestResult {
    let directory = tempfile::tempdir()?;
    let missing = directory.path().join("missing.json");
    let error = global_api::validate(&missing).expect_err("missing contract must fail");
    assert!(matches!(error, ContractError::Read { .. }));
    assert!(error.source().is_some());

    let malformed = directory.path().join("malformed.json");
    fs::write(&malformed, "{")?;
    let error = global_api::validate(&malformed).expect_err("malformed contract must fail");
    assert!(matches!(error, ContractError::Parse { .. }));
    assert!(error.source().is_some());
    Ok(())
}

#[test]
fn fixed_surface_changes_fail_validation() -> TestResult {
    let mut document = contract()?;
    document["openapi"] = json!("3.0.4");
    assert_invalid(&document, "/openapi")?;

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
    document["paths"]["/v1/capabilities"]["get"]["requestBody"] = json!({});
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
    Ok(())
}

#[test]
fn bound_and_shared_schema_changes_fail_validation() -> TestResult {
    let mut document = contract()?;
    document["components"]["parameters"]["Limit"]["schema"]["maximum"] = json!(201);
    assert_invalid(&document, "Limit/schema/maximum")?;

    let mut document = contract()?;
    document["components"]["schemas"]["PackageIdentity"]["properties"]["name"]["maxLength"] =
        json!(4095);
    assert_invalid(&document, "PackageIdentity")?;
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
        checked, 7,
        "each request and success response carries an example"
    );
    Ok(())
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
