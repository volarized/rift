//! Contract checks for the published global package API.

use std::error::Error;
use std::fs;
use std::path::PathBuf;

use rift_cloud_client::contract::{self, ContractError};
use rift_cloud_client::search::PackageSearchPage;
use rift_cloud_client::{
    Capabilities, PackagePatternPage, PackageResolutionResponse, PackageSearchCandidate,
};
use rift_protocol::read::SEARCH_PATTERN_CHARS_MAX;
use serde_json::{Map, Value, json};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn contract() -> Value {
    contract::document()
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
fn generated_contract_is_valid() -> TestResult {
    let (_directory, path) = write_contract(&contract())?;
    contract::validate(&path)?;
    Ok(())
}

/// Every schema `pattern` compiles in the two dialects that read it: the ECMA-262 regular
/// expressions a JSON Schema validator runs, and the `regex` crate the client generator
/// compiles, which has no lookahead. A lookahead the validator accepts fails the contract.
#[test]
fn every_schema_pattern_compiles_for_the_validator_and_the_generator() -> TestResult {
    let document = contract();
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

    let mut document = contract();
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
    let mut document = contract();
    document["openapi"] = json!("3.0.4");
    assert_invalid(&document, "OpenAPI version")?;

    let mut document = contract();
    document["paths"]
        .as_object_mut()
        .ok_or("paths must be an object")?
        .remove("/v1/symbols");
    assert_invalid(&document, "paths differ")?;

    let mut document = contract();
    document["paths"]["/v1/search"]["post"]["operationId"] = json!("search");
    assert_invalid(&document, "operationId")?;

    let mut document = contract();
    document["paths"]["/v1/search"]["post"]["security"] = json!([]);
    assert_invalid(&document, "optional bearer authentication")?;

    let mut document = contract();
    document["servers"] = json!([{"url": "https://example.com/rift/rest"}]);
    assert_invalid(&document, "`servers` must stay absent")?;
    Ok(())
}

#[test]
fn request_and_response_changes_fail_validation() -> TestResult {
    let mut document = contract();
    document["paths"]["/v1/capabilities"]["get"]["requestBody"] =
        document["paths"]["/v1/search"]["post"]["requestBody"].clone();
    assert_invalid(&document, "must not declare a request body")?;

    let mut document = contract();
    document["paths"]["/v1/resolutions"]["post"]["requestBody"]["required"] = json!(false);
    assert_invalid(&document, "request body must be required")?;

    let mut document = contract();
    document["paths"]["/v1/search"]["post"]["responses"]
        .as_object_mut()
        .ok_or("responses must be an object")?
        .remove("504");
    assert_invalid(&document, "response statuses differ")?;

    let mut document = contract();
    document["paths"]["/v1/search"]["post"]["responses"]["400"]["content"] =
        json!({"application/json": {}});
    assert_invalid(&document, "application/problem+json")?;

    let mut document = contract();
    document["paths"]["/v1/capabilities"]["get"]["responses"]["304"]["content"] =
        json!({"application/json": {}});
    assert_invalid(&document, "304 response must not carry content")?;

    let mut document = contract();
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
    let mut document = contract();
    document["components"]["parameters"]["MaxResults"]["schema"]["maximum"] = json!(201);
    assert_invalid(&document, "MaxResults/schema/maximum")?;

    let mut document = contract();
    document["components"]["parameters"]["MaxResults"]["schema"]["default"] = json!(101);
    assert_invalid(&document, "MaxResults/schema/default")?;

    let mut document = contract();
    document["components"]["schemas"]["CapabilityBounds"]["properties"]["page_limit_max"]["maximum"] =
        json!(200);
    assert_invalid(
        &document,
        "CapabilityBounds/properties/page_limit_max/maximum",
    )?;

    let mut document = contract();
    document["components"]["schemas"]["GetSymbolResult"]["oneOf"][0]["properties"]["declarations"]
        ["maxItems"] = json!(200);
    assert_invalid(&document, "GetSymbolResult/found/declarations/maxItems")?;

    let mut document = contract();
    document["components"]["schemas"]["GetSymbolParams"]["properties"]["max_results"]["maximum"] =
        json!(200);
    assert_invalid(&document, "GetSymbolParams/max_results/maximum")?;

    let mut document = contract();
    document["components"]["schemas"]["PackagePatternRequest"]["properties"]["pattern"]["maxLength"] =
        json!(4096);
    assert_invalid(
        &document,
        "PackagePatternRequest/properties/pattern/maxLength",
    )?;

    let mut document = contract();
    document["paths"]["/v1/patterns"]["post"]["x-rift-page-files-max"] = json!(2000);
    assert_invalid(&document, "x-rift-page-files-max")?;

    let mut document = contract();
    document["components"]["schemas"]["FindDeclarationsParams"]["properties"]["positions"]["maxItems"] =
        json!(10000);
    assert_invalid(
        &document,
        "FindDeclarationsParams/properties/positions/maxItems",
    )?;

    let mut document = contract();
    document["components"]["schemas"]["SourcePosition"]["properties"]["character"]["maximum"] =
        json!(65535);
    assert_invalid(&document, "SourcePosition/properties/character/maximum")?;

    let mut document = contract();
    document["components"]["schemas"]["PackageIdentity"]["properties"]["name"]["maxLength"] =
        json!(4095);
    assert_invalid(&document, "PackageIdentity")?;

    let mut document = contract();
    document["components"]["schemas"]["Warning"]["properties"]["detail"]["maxLength"] = json!(1024);
    assert_invalid(&document, "Warning/properties/detail/maxLength")?;

    // A longer requirement makes a longer `requirement_unsatisfied` detail than the warning
    // bound holds.
    let mut document = contract();
    document["components"]["schemas"]["PackageContextEntry"]["properties"]["requirement"]["maxLength"] =
        json!(8192);
    assert_invalid(&document, "Warning/properties/detail/maxLength")?;
    Ok(())
}

/// The contract bounds a package `pattern` by the search pattern bound the local server
/// enforces, and both count characters.
#[test]
fn the_pattern_max_length_is_the_search_pattern_bound() -> TestResult {
    let document = contract();
    let request = &document["components"]["schemas"]["PackagePatternRequest"];
    let max_length = &request["properties"]["pattern"]["maxLength"];
    assert_eq!(
        max_length.as_u64(),
        Some(u64::try_from(SEARCH_PATTERN_CHARS_MAX)?),
        "{max_length}"
    );
    Ok(())
}

/// The contract states the source bound a facets page stops at and the result count bound the
/// client enforces, and a drift in either fails validation.
#[test]
fn the_facets_page_bounds_are_the_ones_the_client_enforces() -> TestResult {
    let mut document = contract();
    document["paths"]["/v1/facets"]["post"]["x-rift-page-source-bytes-max"] = json!(2_097_152);
    assert_invalid(&document, "x-rift-page-source-bytes-max")?;

    let mut document = contract();
    document["components"]["schemas"]["FacetsParams"]["properties"]["max_results"]["maximum"] =
        json!(99);
    assert_invalid(&document, "FacetsParams/max_results/maximum")?;

    let mut document = contract();
    document["paths"]["/v1/facets"]["post"]["requestBody"]["content"]["application/json"]["schema"] =
        json!({"$ref": "#/components/schemas/GetSymbolParams"});
    assert_invalid(&document, "POST /v1/facets request must use FacetsParams")?;

    let mut document = contract();
    document["paths"]["/v1/facets"]["post"]["responses"]["200"]["content"]["application/json"]["schema"] =
        json!({"$ref": "#/components/schemas/GetSymbolResult"});
    assert_invalid(
        &document,
        "POST /v1/facets 200 response must use FacetsResult",
    )?;

    let mut document = contract();
    document["paths"]["/v1/facets"]["post"]["parameters"] = json!([]);
    assert_invalid(
        &document,
        "POST /v1/facets must not declare query selectors",
    )?;
    Ok(())
}

/// An optional property accepting `null` fails validation, in a shared schema and in one the
/// contract alone defines: the service omits an absent field.
#[test]
fn an_optional_property_accepting_null_fails_validation() -> TestResult {
    let mut document = contract();
    document["components"]["schemas"]["PackageSearchPage"]["properties"]["documentation_revision"]
        ["type"] = json!(["string", "null"]);
    assert_invalid(&document, "schema `PackageSearchPage` accepts `null`")?;

    let mut document = contract();
    document["components"]["schemas"]["Symbol"]["properties"]["visibility"]["type"] =
        json!(["string", "null"]);
    assert_invalid(&document, "schema `Symbol` accepts `null`")?;
    Ok(())
}

/// A boolean component schema holds no property, so the `null` rule has nothing to refuse
/// in it and the contract stays valid.
#[test]
fn a_boolean_component_schema_passes_the_optional_null_rule() -> TestResult {
    let mut document = contract();
    document["components"]["schemas"]["AnyValue"] = json!(true);
    let (_directory, path) = write_contract(&document)?;
    contract::validate(&path)?;
    Ok(())
}

#[test]
fn every_contract_example_validates_against_its_schema() -> TestResult {
    let document = contract();
    let components = document["components"]["schemas"]
        .as_object()
        .ok_or("components.schemas must be an object")?;
    let mut checked = 0_usize;
    validate_examples(&document, components, &mut checked)?;
    assert_eq!(
        checked, 20,
        "each request and success response carries an example"
    );
    Ok(())
}

#[test]
fn every_contract_response_example_decodes_through_generated_types() -> TestResult {
    let document = contract();
    let mut checked = 0_usize;
    decode_examples(&document, &mut checked)?;
    assert_eq!(
        checked, 11,
        "each success response carries a generated response type"
    );
    Ok(())
}

#[test]
fn search_request_examples_carry_the_terms_and_identifiers_of_their_query() -> TestResult {
    let document = contract();
    let examples = document["paths"]["/v1/search"]["post"]["requestBody"]["content"]
        ["application/json"]["examples"]
        .as_object()
        .ok_or("the search operation carries request examples")?;
    let mut checked = 0_usize;
    for example in examples.values() {
        let body = &example["value"];
        let Some(text) = body["query"].as_str() else {
            continue;
        };
        let query = rift_ranking::ParsedQuery::parse(text)?;
        let terms = query
            .members()
            .iter()
            .map(|member| json!({"text": member.text(), "phrase": member.is_phrase(), "prefix": false}))
            .collect::<Vec<_>>();
        let identifiers = query
            .candidates()
            .iter()
            .map(|candidate| json!(candidate.text()))
            .collect::<Vec<_>>();
        assert_eq!(body["terms"], json!(terms), "terms of {text:?}");
        assert_eq!(
            body["identifiers"],
            json!(identifiers),
            "identifiers of {text:?}"
        );
        checked += 1;
    }
    assert_eq!(checked, 2, "each lexical search example is checked");
    Ok(())
}

fn decode_examples(syntax_element: &Value, checked: &mut usize) -> TestResult {
    match syntax_element {
        Value::Object(object) => {
            if let (Some(schema), Some(examples)) = (
                object.get("schema"),
                object.get("examples").and_then(Value::as_object),
            ) {
                for example in examples.values() {
                    let value = example
                        .get("value")
                        .ok_or("a contract example must carry a value")?;
                    let decoded =
                        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
                            decode_generated_response_example(reference, value.clone())?
                        } else if schema
                            == &json!({"oneOf":[
                                {"$ref":"#/components/schemas/PackageSearchPage"},
                                {"$ref":"#/components/schemas/EdgeSearchPage"}
                            ]})
                        {
                            let generated: rift_cloud_client::Search200Response =
                                serde_json::from_value(value.clone())?;
                            match generated {
                                rift_cloud_client::Search200Response::Package(_) => {
                                    decode_generated_response_example(
                                        "#/components/schemas/PackageSearchPage",
                                        value.clone(),
                                    )?;
                                }
                                rift_cloud_client::Search200Response::Edge(_) => {
                                    let page: rift_protocol::edge_read::EdgeSearchPage =
                                        serde_json::from_value(value.clone())?;
                                    assert!(page.groups.iter().all(|group| group.complete));
                                }
                            }
                            true
                        } else {
                            false
                        };
                    if decoded {
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
        "#/components/schemas/GetSymbolResult" => {
            let result =
                serde_json::from_value::<rift_protocol::symbol_read::GetSymbolResult>(value)?;
            let identity = rift_protocol::read::SymbolId::parse(
                "rift://symbol/cargo/crates.io/itoa/rust/itoa/Buffer",
            )?;
            assert!(result.is_valid_for(&identity));
        }
        "#/components/schemas/SearchUnavailable" => {
            serde_json::from_value::<rift_cloud_client::SearchUnavailable>(value)?;
        }
        "#/components/schemas/PackagePatternPage" => {
            serde_json::from_value::<PackagePatternPage>(value)?;
        }
        "#/components/schemas/FacetsResult" => {
            let result = serde_json::from_value::<rift_protocol::read::FacetsResult>(value)?;
            assert!(
                result
                    .declarations
                    .iter()
                    .flat_map(|declaration| &declaration.syntax)
                    .all(|syntax_element| !syntax_element.unit.is_project())
            );
        }
        "#/components/schemas/FindDeclarationsResult" => {
            serde_json::from_value::<rift_protocol::source_read::FindDeclarationsResult>(value)?;
        }
        "#/components/schemas/PackageResolutionRequest"
        | "#/components/schemas/PackageSearchRequest"
        | "#/components/schemas/PackagePatternRequest"
        | "#/components/schemas/GetSymbolParams"
        | "#/components/schemas/FacetsParams"
        | "#/components/schemas/FindDeclarationsParams" => return Ok(false),
        other => return Err(format!("contract example names no generated type: {other}").into()),
    }
    Ok(true)
}

fn validate_examples(
    syntax_element: &Value,
    components: &Map<String, Value>,
    checked: &mut usize,
) -> TestResult {
    match syntax_element {
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

#[test]
fn collection_schema_ceilings_do_not_revert_to_runtime_defaults() -> TestResult {
    for (property, old_default) in [
        ("request_body_bytes_max", 4 << 20),
        ("response_body_bytes_max", 32 << 20),
        ("source_bytes_max", 1 << 20),
    ] {
        let mut document = contract();
        document["components"]["schemas"]["CapabilityBounds"]["properties"][property]["maximum"] =
            json!(old_default);
        assert_invalid(
            &document,
            &format!("CapabilityBounds/properties/{property}/maximum"),
        )?;
    }
    for component in [
        "PackageSearchHit",
        "PackagePatternDeclaration",
        "PackagePatternHit",
    ] {
        let mut document = contract();
        document["components"]["schemas"][component]["properties"]["source"]["maxLength"] =
            json!(1 << 20);
        assert_invalid(
            &document,
            &format!("{component}/properties/source/maxLength"),
        )?;
    }
    Ok(())
}

#[test]
fn exact_symbol_contract_rejects_name_selectors_and_legacy_pages() -> TestResult {
    let mut document = contract();
    document["paths"]["/v1/symbols"]["post"]["requestBody"]["content"]["application/json"]["schema"] =
        json!({"$ref": "#/components/schemas/PackageSymbolRequest"});
    assert_invalid(&document, "request must use GetSymbolParams")?;

    let mut document = contract();
    document["paths"]["/v1/symbols"]["post"]["parameters"] =
        json!([{"$ref": "#/components/parameters/Limit"}]);
    assert_invalid(&document, "must not declare query selectors")?;

    for status in ["200", "404", "503"] {
        let mut document = contract();
        document["paths"]["/v1/symbols"]["post"]["responses"][status]["content"]["application/json"]
            ["schema"] = json!({"$ref": "#/components/schemas/PackageSymbolPage"});
        assert_invalid(
            &document,
            &format!("{status} response must use GetSymbolResult"),
        )?;
    }
    Ok(())
}

#[test]
fn exact_symbol_contract_requires_typed_miss_and_unavailability() -> TestResult {
    let mut document = contract();
    document["paths"]["/v1/symbols"]["post"]["responses"]
        .as_object_mut()
        .ok_or("responses must be an object")?
        .remove("404");
    assert_invalid(&document, "response statuses differ")?;

    let mut document = contract();
    document["paths"]["/v1/symbols"]["post"]["responses"]["503"]["content"]
        .as_object_mut()
        .ok_or("response content must be an object")?
        .remove("application/json");
    assert_invalid(&document, "must declare typed outcomes and problem details")?;

    let mut document = contract();
    document["components"]["schemas"]["GetSymbolParams"]["required"] = json!([]);
    assert_invalid(&document, "shared schema `GetSymbolParams` differs")?;

    let mut document = contract();
    document["components"]["schemas"]["GetSymbolResult"]["oneOf"]
        .as_array_mut()
        .ok_or("symbol outcomes must be an array")?
        .truncate(1);
    assert_invalid(&document, "shared schema `GetSymbolResult` differs")?;
    Ok(())
}

#[test]
fn merged_edges_validate_with_optional_provider_sets() -> TestResult {
    use rift_protocol::edge_read::EdgeInclude;
    use rift_protocol::index::StoredEdge;
    let document = contract();
    let schema = json!({"$ref":"#/components/schemas/Edge", "components": document["components"]});
    let validator = jsonschema::validator_for(&schema)?;
    let mut rows = Vec::new();
    for provider in ["static", "engine", "lsp"] {
        rows.push(serde_json::from_value::<StoredEdge>(json!({
            "from":"rift://symbol/cargo/crates.io/demo/rust/caller",
            "to":"rift://symbol/cargo/crates.io/demo/rust/callee",
            "kind":"calls", "kinds":["calls"], "provider":provider, "derivation":"resolution"
        }))?);
    }
    for included in [false, true] {
        let edges = StoredEdge::project(
            rows.clone(),
            EdgeInclude {
                providers: included,
                ..Default::default()
            },
        );
        assert_eq!(edges.len(), 1);
        let mut edge = serde_json::to_value(&edges[0])?;
        assert!(validator.is_valid(&edge), "{edge}");
        let decoded: rift_cloud_client::Search200Response = serde_json::from_value(json!({
            "groups": [{"seed":edge["from"], "complete":true, "edges":[{"id":"a".repeat(64), "edge":edge}]}],
            "analyzer_revision":"fixture", "corpus_revision":"fixture",
            "view":{"id":"fixture.view", "expires_at":"2099-01-01T00:00:00Z"}
        }))?;
        let rift_cloud_client::Search200Response::Edge(page) = decoded else {
            panic!("edge page")
        };
        assert_eq!(
            serde_json::to_value(&page.groups[0].edges[0].edge)?.get("providers"),
            included
                .then(|| json!(["engine", "lsp", "static"]))
                .as_ref()
        );
        edge["provider"] = json!("static");
        assert!(
            !validator.is_valid(&edge),
            "singular provider leaves the wire"
        );
        edge.as_object_mut()
            .ok_or("edge object")?
            .remove("provider");
        edge["providers"] = json!(["static", "static"]);
        assert!(!validator.is_valid(&edge), "provider set is unique");
        edge["providers"] = json!(["unknown"]);
        assert!(
            !validator.is_valid(&edge),
            "provider set uses the declared analyses"
        );
    }
    Ok(())
}

#[test]
fn traversal_search_accepts_typed_provider_projection() -> TestResult {
    let document = contract();
    let schema = json!({"$ref":"#/components/schemas/PackageSearchRequest", "components":document["components"]});
    let validator = jsonschema::validator_for(&schema)?;
    let mut request = rift_cloud_client::PackageSearchRequest {
        include: Some(rift_cloud_client::PackageSearchRequestInclude {
            providers: Some(true),
            source: None,
        }),
        ..Default::default()
    };
    request.traversal.get_or_insert_default().seed =
        Some("rift://symbol/cargo/crates.io/demo/rust/caller".to_owned());
    let mut value = serde_json::to_value(request)?;
    assert!(validator.is_valid(&value), "{value}");
    assert_eq!(value["include"], json!({"providers":true}));
    value["include"] = json!(["providers"]);
    assert!(!validator.is_valid(&value));
    value["include"] = json!({"providers":"true"});
    assert!(!validator.is_valid(&value));
    Ok(())
}
