use serde_json::{Value, json};

use super::{CapturedViewId, GetSymbolParams, GetSymbolResult, SymbolDeclaration};
use crate::read::{GetSymbolInclude, Symbol, SymbolId};

const ID: &str = "rift://symbol/local/rust/app/parser/parse";
const VIEW: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn symbol() -> Value {
    json!({
        "id": ID,
        "language": "rust",
        "name": "parse",
        "kind": "function",
        "types": [{"role": "return", "origin": "declared", "type": {"language": "rust", "source": "Result<(), Error>"}}],
        "signatures": [{"display": "fn parse()", "language": "rust"}],
        "documentation": [{"format": "markdown", "text": "Parses input."}]
    })
}

fn declaration(path: &str, signature: u64, documentation: u64) -> Value {
    json!({
        "origin": {"location": "project", "source_kind": "authored"},
        "path": path,
        "range": {"start": 0, "end": 12},
        "line": 1,
        "source": "fn parse() {}",
        "source_complete": true,
        "signature_indices": [signature],
        "type_indices": [0],
        "documentation_indices": [documentation]
    })
}

#[test]
fn exact_requests_validate_identity_and_reject_removed_fields() {
    let request: GetSymbolParams =
        serde_json::from_value(json!({"id": ID})).expect("valid fixture");
    assert_eq!(request.id.as_str(), ID);
    assert_eq!(request.include, [GetSymbolInclude::Source]);
    assert_eq!(request.declaration_limit, 5);
    assert_eq!(request.declaration_cursor, None);
    assert_eq!(request.rev, None);
    assert_eq!(request.view, None);
    let empty: GetSymbolParams =
        serde_json::from_value(json!({"id": ID, "include": []})).expect("valid fixture");
    assert!(empty.include.is_empty());

    for field in [
        "name",
        "scope",
        "packages",
        "language",
        "limit",
        "page_index",
    ] {
        let mut value = json!({"id": ID});
        value[field] = json!("removed");
        assert!(
            serde_json::from_value::<GetSymbolParams>(value).is_err(),
            "{field}"
        );
    }
    for malformed in [
        "parse",
        "rift://symbol/rust/src/lib.rs/parse",
        "rift://symbol/local/rust/app/%70arse",
    ] {
        assert!(serde_json::from_value::<GetSymbolParams>(json!({"id": malformed})).is_err());
    }
    assert!(serde_json::from_value::<GetSymbolParams>(json!({})).is_err());
}

#[test]
fn captured_view_requires_complete_key_and_unknown_fields_are_refused() {
    let view = CapturedViewId::parse(VIEW).expect("valid fixture");
    assert_eq!(view.as_str(), VIEW);
    let value = serde_json::to_value(&view).expect("valid fixture");
    assert_eq!(value, json!(VIEW));
    assert_eq!(
        serde_json::from_value::<CapturedViewId>(value).expect("valid fixture"),
        view
    );
    for refused in [
        String::new(),
        VIEW[..8].to_owned(),
        VIEW.to_uppercase(),
        "g".repeat(64),
        "0".repeat(65),
    ] {
        let error = CapturedViewId::parse(&refused).expect_err("invalid view key");
        assert_eq!(error.slug(), rift_error::errors::server::read_invalid::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| key == "field" && value == "view")
        );
        assert!(serde_json::from_value::<CapturedViewId>(json!(refused)).is_err());
    }
    assert!(
        serde_json::from_value::<GetSymbolParams>(json!({"id": ID, "view": "01234567"})).is_err()
    );
}

#[test]
fn captured_view_accepts_bounded_opaque_keys_without_claiming_authentication() {
    for key in ["claims.signature", "a_B-9.z_Y-0"] {
        let view = CapturedViewId::parse(key).expect("opaque view spelling");
        assert_eq!(view.as_str(), key);
        let encoded = serde_json::to_value(&view).expect("view encoding");
        assert_eq!(
            serde_json::from_value::<CapturedViewId>(encoded).expect("view decoding"),
            view
        );
    }
    let exact = format!("{}.s", "c".repeat(4094));
    assert_eq!(exact.len(), 4096);
    assert!(CapturedViewId::parse(&exact).is_ok());
    for refused in [
        format!("{exact}s"),
        ".signature".to_owned(),
        "claims.".to_owned(),
        "claims.signature.extra".to_owned(),
        "claims.signature=".to_owned(),
        "claims.sig nature".to_owned(),
        "claims.é".to_owned(),
    ] {
        assert!(CapturedViewId::parse(&refused).is_err());
        assert!(serde_json::from_value::<CapturedViewId>(json!(refused)).is_err());
    }
    let schema = serde_json::to_value(schemars::schema_for!(CapturedViewId)).expect("view schema");
    assert_eq!(schema["maxLength"], 4096);
    assert_eq!(schema["minLength"], 3);
}

#[test]
fn declaration_projection_references_one_object_and_keeps_source_association() {
    let mut value = symbol();
    value["signatures"]
        .as_array_mut()
        .expect("valid fixture")
        .push(json!({"display": "fn parse(input: &str)", "language": "rust"}));
    value["documentation"]
        .as_array_mut()
        .expect("valid fixture")
        .push(json!({"format": "markdown", "text": "Parses text."}));
    let symbol: Symbol = serde_json::from_value(value).expect("valid fixture");
    let first: SymbolDeclaration =
        serde_json::from_value(declaration("src/parse.rs", 0, 0)).expect("valid fixture");
    let second: SymbolDeclaration =
        serde_json::from_value(declaration("src/parse.pyi", 1, 1)).expect("valid fixture");
    assert!(first.is_valid_for(&symbol));
    assert!(second.is_valid_for(&symbol));
    assert_eq!(
        symbol.signatures[usize::try_from(first.signature_indices[0]).expect("valid fixture")]
            .display,
        "fn parse()"
    );
    assert_eq!(
        symbol.signatures[usize::try_from(second.signature_indices[0]).expect("valid fixture")]
            .display,
        "fn parse(input: &str)"
    );
    assert_eq!(
        symbol.documentation
            [usize::try_from(second.documentation_indices[0]).expect("valid fixture")]
        .text,
        "Parses text."
    );
    assert_ne!(first.path, second.path);
    assert_eq!(
        symbol.types[usize::try_from(first.type_indices[0]).expect("valid type index")],
        symbol.types[usize::try_from(second.type_indices[0]).expect("valid type index")]
    );

    for (field, invalid) in [
        ("signature_indices", json!([2])),
        ("signature_indices", json!([u64::MAX])),
        ("signature_indices", json!([1, 0])),
        ("signature_indices", json!([0, 0])),
        ("documentation_indices", json!([2])),
        ("type_indices", json!([1])),
        ("line", json!(0)),
        ("range", json!({"start": 13, "end": 12})),
        ("source", Value::Null),
    ] {
        let mut value = declaration("src/parse.rs", 0, 0);
        value[field] = invalid;
        let binding: SymbolDeclaration = serde_json::from_value(value).expect("valid fixture");
        assert!(!binding.is_valid_for(&symbol), "{field}");
    }
    let mut neither = declaration("src/parse.rs", 0, 0);
    neither
        .as_object_mut()
        .expect("valid fixture")
        .remove("path");
    let binding: SymbolDeclaration = serde_json::from_value(neither).expect("valid fixture");
    assert!(!binding.is_valid_for(&symbol));
    let mut both = declaration("src/parse.rs", 0, 0);
    both["unit"] = json!("rift://source/project/src/parse.rs");
    let binding: SymbolDeclaration = serde_json::from_value(both).expect("valid fixture");
    assert!(!binding.is_valid_for(&symbol));
}

#[test]
fn source_less_success_stays_distinct_from_missing_and_unavailable() {
    let requested = SymbolId::parse(ID).expect("valid fixture");
    let found = json!({
        "outcome": "found",
        "symbol": symbol(),
        "view": {"id": VIEW, "expires_at": "2026-10-10T12:00:00Z"},
        "declarations": []
    });
    let result: GetSymbolResult = serde_json::from_value(found.clone()).expect("valid fixture");
    assert!(result.is_valid_for(&requested));
    assert_eq!(serde_json::to_value(result).expect("valid fixture"), found);
    for altered in [Value::Null, json!("rift://symbol/local/rust/app/other")] {
        let mut value = found.clone();
        value["symbol"]["id"] = altered;
        let result: GetSymbolResult = serde_json::from_value(value).expect("valid fixture");
        assert!(!result.is_valid_for(&requested));
    }
    let mut wrong_language = found;
    wrong_language["symbol"]["language"] = json!("typescript");
    assert!(
        !serde_json::from_value::<GetSymbolResult>(wrong_language)
            .expect("valid fixture")
            .is_valid_for(&requested)
    );

    let missing = json!({
        "outcome": "missing",
        "symbol_not_found": {"id": ID, "alternatives": [{
            "id": "rift://symbol/local/rust/app/parser/read",
            "name": "read",
            "match_context": ["module", "name"]
        }]},
        "view": {"id": VIEW, "expires_at": "2026-10-10T12:00:00Z"}
    });
    let result: GetSymbolResult = serde_json::from_value(missing.clone()).expect("valid fixture");
    assert!(result.is_valid_for(&requested));
    assert_eq!(
        serde_json::to_value(result).expect("valid fixture"),
        missing
    );
    let unavailable = json!({"outcome": "unavailable", "id": ID, "reason": "view_expired"});
    let result: GetSymbolResult =
        serde_json::from_value(unavailable.clone()).expect("valid fixture");
    assert!(result.is_valid_for(&requested));
    assert_eq!(
        serde_json::to_value(result).expect("valid fixture"),
        unavailable
    );
}

#[test]
fn exact_request_schema_advertises_codec_owners_and_declaration_bounds() {
    let schema = serde_json::to_value(schemars::schema_for!(GetSymbolParams)).expect("schema");
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    for id in [
        ID,
        "rift://symbol/local@cloud/rust/rift_cloud_service/resolution/resolve_entries",
        "rift://symbol/npm/npmjs.org/@types/node@26.6.4/typescript/node/buffer/Buffer",
        "rift://symbol/stdlib/cpython@3.12.9/python/builtins/len",
        "rift://symbol/cargo/packages.example%2Freleases/demo@1.2.3/rust/demo/run",
    ] {
        SymbolId::parse(id).expect("canonical fixture");
        assert!(validator.is_valid(&json!({"id": id})), "{id}");
    }
    for value in [
        json!({"id": "rift://symbol/rust/src/lib.rs/parse"}),
        json!({"id": ID, "declaration_limit": 0}),
        json!({"id": ID, "declaration_limit": 10001}),
        json!({"id": ID, "declaration_cursor": ""}),
        json!({"id": ID, "name": "parse"}),
    ] {
        assert!(!validator.is_valid(&value), "{value}");
    }
    let view_schema = serde_json::to_value(schemars::schema_for!(CapturedViewId)).expect("schema");
    let view_validator = jsonschema::validator_for(&view_schema).expect("schema compiles");
    assert!(view_validator.is_valid(&json!(VIEW)));
    assert!(!view_validator.is_valid(&json!(VIEW[..8].to_owned())));
    assert!(!view_validator.is_valid(&json!(VIEW.to_uppercase())));
}
