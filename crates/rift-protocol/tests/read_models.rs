//! Read-tool wire model tests against canonical JSON fixtures.

use std::collections::BTreeSet;
use std::error::Error;

use rift_protocol::read::{
    Coverage, CoverageScope, FileContent, Filter, GetSymbolParams, GetSymbolResult, NodesParams,
    NodesResult, SearchHitTarget, SearchParams, SearchResult, SourceLocation,
};
use rift_protocol::schema::schema_document;
use schemars::{JsonSchema, schema_for};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn Error>>;

fn exported_definition(name: &str) -> Result<Value, Box<dyn Error>> {
    let document: Value = serde_json::from_str(&schema_document())?;
    let definition = document
        .get("$defs")
        .and_then(Value::as_object)
        .and_then(|definitions| definitions.get(name))
        .ok_or_else(|| format!("exported schema has no {name} definition"))?;
    assert!(definition.is_object(), "{name} must be an object schema");
    Ok(definition.clone())
}

fn object_keys(value: &Value, field: &str) -> Result<BTreeSet<String>, String> {
    value
        .get(field)
        .and_then(Value::as_object)
        .map(|object| object.keys().cloned().collect())
        .ok_or_else(|| format!("schema field {field} must be an object"))
}

fn required_fields(value: &Value) -> Result<BTreeSet<String>, String> {
    let Some(required) = value.get("required") else {
        return Ok(BTreeSet::new());
    };
    required
        .as_array()
        .ok_or_else(|| "schema required field must be an array".to_owned())?
        .iter()
        .map(|field| {
            field
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "required entries must be strings".to_owned())
        })
        .collect()
}

fn assert_root_object<T: JsonSchema>(name: &str) -> TestResult {
    let exported = exported_definition(name)?;
    let generated = serde_json::to_value(schema_for!(T))?;

    assert_eq!(generated.get("type"), exported.get("type"));
    assert_eq!(
        generated.get("additionalProperties"),
        exported.get("additionalProperties")
    );
    assert_eq!(
        object_keys(&generated, "properties")?,
        object_keys(&exported, "properties")?
    );
    assert_eq!(required_fields(&generated)?, required_fields(&exported)?);
    assert_eq!(generated.get("allOf"), exported.get("allOf"));
    assert_eq!(generated.get("anyOf"), exported.get("anyOf"));
    Ok(())
}

fn assert_rejected<T>(value: Value)
where
    T: serde::de::DeserializeOwned,
{
    assert!(
        serde_json::from_value::<T>(value).is_err(),
        "invalid wire value must be rejected"
    );
}

#[test]
fn nodes_roots_match_exported_object_shapes() -> TestResult {
    assert_root_object::<NodesParams>("NodesParams")?;
    assert_root_object::<NodesResult>("NodesResult")
}

#[test]
fn get_symbol_roots_match_exported_object_shapes() -> TestResult {
    assert_root_object::<GetSymbolParams>("GetSymbolParams")?;
    assert_root_object::<GetSymbolResult>("GetSymbolResult")
}

#[test]
fn get_symbol_params_apply_canonical_defaults_and_close_root() -> TestResult {
    let params: GetSymbolParams = serde_json::from_value(json!({"name": "BaseModel"}))?;
    let encoded = serde_json::to_value(params)?;
    assert_eq!(encoded["name"], "BaseModel");
    assert_eq!(encoded["include_body"], true);
    assert_eq!(encoded["include_history"], false);
    assert_eq!(encoded["limit"], 5);
    assert_eq!(encoded["scope"], "all");

    assert_rejected::<GetSymbolParams>(json!({}));
    assert_rejected::<GetSymbolParams>(json!({"name": "BaseModel", "extra": true}));
    Ok(())
}

#[test]
fn get_symbol_result_requires_present_nullable_cursor() -> TestResult {
    let result = json!({
        "hits": [],
        "coverage": {
            "state": "complete",
            "scope": {"kind": "reach", "reach": "request"},
            "origins": []
        },
        "next_cursor": null,
        "snapshot": {
            "tree_revision": "0".repeat(64),
            "index": null,
            "source_revision": "0".repeat(64)
        }
    });
    let decoded: GetSymbolResult = serde_json::from_value(result.clone())?;
    assert_eq!(serde_json::to_value(decoded)?, result);

    let mut missing_cursor = result;
    missing_cursor
        .as_object_mut()
        .ok_or("get-symbol result must be an object")?
        .remove("next_cursor");
    assert_rejected::<GetSymbolResult>(missing_cursor);
    Ok(())
}

#[test]
fn source_location_accepts_only_closed_known_variants() -> TestResult {
    let accepted = [
        json!({"kind": "project"}),
        json!({"kind": "dependency", "package": {
            "manager": "cargo",
            "name": "serde",
            "version": "1.0.229"
        }}),
        json!({"kind": "stdlib"}),
        json!({"kind": "external"}),
    ];
    for value in accepted {
        let decoded: SourceLocation = serde_json::from_value(value.clone())?;
        let mut encoded = serde_json::to_value(decoded)?;
        if value == json!({"kind": "project"}) {
            encoded
                .as_object_mut()
                .ok_or("project source location must serialize as object")?
                .remove("package");
        }
        assert_eq!(encoded, value);
    }

    assert_rejected::<SourceLocation>(json!({"kind": "dependency"}));
    assert_rejected::<SourceLocation>(json!({"kind": "unknown"}));
    assert_rejected::<SourceLocation>(json!({"kind": "stdlib", "package": null}));
    Ok(())
}

#[test]
fn search_roots_match_exported_object_shapes() -> TestResult {
    assert_root_object::<SearchParams>("SearchParams")?;
    assert_root_object::<SearchResult>("SearchResult")
}

#[test]
fn search_params_apply_defaults_and_close_root() -> TestResult {
    let params: SearchParams = serde_json::from_value(json!({"query": "BaseModel"}))?;
    let encoded = serde_json::to_value(params)?;
    assert_eq!(encoded["query"], "BaseModel");
    assert_eq!(encoded["target"], "all");
    assert_eq!(encoded["order"], "relevance");
    assert_eq!(encoded["scope"], "project");

    assert_rejected::<SearchParams>(json!("BaseModel"));
    assert_rejected::<SearchParams>(json!({"query": "BaseModel", "extra": true}));
    Ok(())
}

#[test]
fn search_result_requires_present_nullable_cursor() -> TestResult {
    let result = json!({
        "snapshot": {
            "tree_revision": "0".repeat(64),
            "index": null,
            "source_revision": "0".repeat(64)
        },
        "coverage": {
            "state": "complete",
            "scope": {"kind": "reach", "reach": "request"},
            "origins": []
        },
        "results": [],
        "next_cursor": null
    });
    let decoded: SearchResult = serde_json::from_value(result.clone())?;
    assert_eq!(serde_json::to_value(decoded)?, result);

    let mut missing_cursor = result;
    missing_cursor
        .as_object_mut()
        .ok_or("search result must be an object")?
        .remove("next_cursor");
    assert_rejected::<SearchResult>(missing_cursor);
    Ok(())
}

#[test]
fn filter_accepts_recursive_closed_variants() -> TestResult {
    let field = json!({
        "kind": "field",
        "field": {"field": "name", "op": "eq", "value": "BaseModel"}
    });
    let accepted = [
        field.clone(),
        json!({
            "kind": "relation",
            "relation": {"kind": ["calls"], "direction": "outgoing"}
        }),
        json!({"kind": "all", "all": [field.clone()]}),
        json!({"kind": "any", "any": [field.clone()]}),
        json!({"kind": "not", "not": field.clone()}),
        json!({
            "kind": "element",
            "element": {"field": "types", "where": field.clone()}
        }),
    ];
    for value in accepted {
        serde_json::from_value::<Filter>(value)?;
    }

    assert_rejected::<Filter>(json!({"kind": "unknown"}));
    assert_rejected::<Filter>(json!({"kind": "field"}));
    assert_rejected::<Filter>(json!({
        "kind": "field",
        "field": {"field": "name", "op": "eq", "extra": true}
    }));
    Ok(())
}

#[test]
fn file_content_accepts_only_closed_known_variants() -> TestResult {
    let regular = json!({"kind": "regular", "size": 17, "executable": false});
    let symlink = json!({"kind": "symlink", "target": "c3JjL2xpYi5ycw=="});
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<FileContent>(regular.clone())?)?,
        regular
    );
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<FileContent>(symlink.clone())?)?,
        symlink
    );

    assert_rejected::<FileContent>(json!({"kind": "regular", "size": 17}));
    assert_rejected::<FileContent>(json!({"kind": "directory"}));
    assert_rejected::<FileContent>(json!({
        "kind": "symlink",
        "target": "c3JjL2xpYi5ycw==",
        "extra": true
    }));
    Ok(())
}

#[test]
fn search_hit_target_accepts_only_closed_known_variants() -> TestResult {
    let file = json!({
        "target": "file",
        "file": {
            "id": "rift://file/src/lib.rs",
            "content": {"kind": "regular", "size": 17, "executable": false},
            "languages": [],
            "regions": [],
            "semantic": true
        }
    });
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<SearchHitTarget>(file.clone())?)?,
        file
    );

    assert_rejected::<SearchHitTarget>(json!({"target": "file"}));
    assert_rejected::<SearchHitTarget>(json!({"target": "unknown"}));
    assert_rejected::<SearchHitTarget>(json!({
        "target": "file",
        "file": {
            "id": "rift://file/src/lib.rs",
            "content": {"kind": "regular", "size": 17, "executable": false},
            "languages": [],
            "regions": [],
            "semantic": true
        },
        "extra": true
    }));
    Ok(())
}

#[test]
fn nodes_params_accepts_only_closed_root_objects() -> TestResult {
    let params: NodesParams = serde_json::from_value(json!({
        "path": "src/lib.rs",
        "position": 17
    }))?;
    let encoded = serde_json::to_value(params)?;
    assert_eq!(encoded["path"], "src/lib.rs");
    assert_eq!(encoded["position"], 17);
    assert_eq!(encoded["projection"], Value::Null);

    assert_rejected::<NodesParams>(json!([]));
    assert_rejected::<NodesParams>(json!({"path": "src/lib.rs"}));
    assert_rejected::<NodesParams>(json!({"position": 17}));
    assert_rejected::<NodesParams>(json!({
        "path": "src/lib.rs",
        "position": 17,
        "extra": true
    }));
    Ok(())
}

#[test]
fn coverage_scope_accepts_only_closed_known_variants() -> TestResult {
    let reach = json!({"kind": "reach", "reach": "request"});
    let unit = json!({"kind": "unit", "unit": "rift://source/rust/src/lib.rs"});
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<CoverageScope>(reach.clone())?)?,
        reach
    );
    assert_eq!(
        serde_json::to_value(serde_json::from_value::<CoverageScope>(unit.clone())?)?,
        unit
    );

    assert_rejected::<CoverageScope>(json!({"kind": "reach"}));
    assert_rejected::<CoverageScope>(json!({"kind": "file", "unit": "x"}));
    assert_rejected::<CoverageScope>(json!({
        "kind": "reach",
        "reach": "request",
        "unit": "x"
    }));
    Ok(())
}

#[test]
fn coverage_accepts_only_closed_state_shapes() -> TestResult {
    let scope = json!({"kind": "reach", "reach": "request"});
    let accepted = [
        json!({
            "state": "complete",
            "scope": scope,
            "origins": []
        }),
        json!({
            "state": "partial",
            "scope": scope,
            "reason": "page boundary",
            "continuation": null,
            "origins": []
        }),
        json!({
            "state": "unsupported",
            "scope": scope,
            "reason": "provider lacks nodes",
            "origins": []
        }),
    ];
    for value in accepted {
        let decoded: Coverage = serde_json::from_value(value.clone())?;
        assert_eq!(serde_json::to_value(decoded)?, value);
    }

    assert_rejected::<Coverage>(json!({
        "state": "partial",
        "scope": scope,
        "origins": []
    }));
    assert_rejected::<Coverage>(json!({
        "state": "unknown",
        "scope": scope,
        "reason": "unknown state",
        "origins": []
    }));
    assert_rejected::<Coverage>(json!({
        "state": "complete",
        "scope": scope,
        "origins": [],
        "extra": true
    }));
    Ok(())
}
