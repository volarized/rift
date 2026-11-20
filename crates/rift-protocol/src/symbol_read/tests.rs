use serde_json::{Value, json};

use super::{CapturedViewId, GetSymbolParams, GetSymbolResult, SymbolDeclaration};
use crate::read::{GetSymbolInclude, Symbol, SymbolId};

const ID: &str = "rift://symbol/local/rust/app/parser/parse";
const VIEW: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const COMMIT_REVISION: &str = "rift://revision/git/1f2080e49da12fee4431e6872630509355cd62d1";

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
    assert!(request.include.is_empty());
    assert_eq!(request.max_results, 10);
    assert_eq!(request.page, 1);
    assert_eq!(request.revision, None);
    let empty: GetSymbolParams =
        serde_json::from_value(json!({"id": ID, "include": {}})).expect("valid fixture");
    assert!(empty.include.is_empty());
    let history: GetSymbolParams =
        serde_json::from_value(json!({"id": ID, "include": {"history": true}}))
            .expect("valid fixture");
    assert_eq!(
        history.include,
        GetSymbolInclude {
            history: true,
            ..GetSymbolInclude::default()
        }
    );
    let source =
        serde_json::from_value::<GetSymbolParams>(json!({"id": ID, "include": {"source": true}}))
            .expect_err("a declaration's source is read by `facets`, never through `include`");
    assert!(
        source.to_string().contains("unknown field `source`"),
        "{source}"
    );
    serde_json::from_value::<GetSymbolParams>(json!({"id": ID, "include": ["history"]}))
        .expect_err("`include` is an object, never an array");

    for field in ["name", "scope", "packages", "language", "limit", "view"] {
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
fn declaration_projection_references_one_object() {
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
        "revision": COMMIT_REVISION,
        "declarations": [], "pagination":{"page":1,"pages":0,"max_results":10,"total":0}
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
    let mut released = found.clone();
    released["revision"] = json!("rift://revision/cargo/crates.io/app@1.0.0");
    assert!(
        !serde_json::from_value::<GetSymbolResult>(released)
            .expect("valid fixture")
            .is_valid_for(&requested),
        "a project answer never reports a release revision"
    );
    let unparsed = SymbolId("rift://symbol/local".to_owned());
    assert!(
        !serde_json::from_value::<GetSymbolResult>(found.clone())
            .expect("valid fixture")
            .is_valid_for(&unparsed),
        "an unparsed request identity"
    );
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
        "revision": COMMIT_REVISION
    });
    let result: GetSymbolResult = serde_json::from_value(missing.clone()).expect("valid fixture");
    assert!(result.is_valid_for(&requested));
    assert_eq!(
        serde_json::to_value(result).expect("valid fixture"),
        missing
    );
    let unavailable = json!({"outcome": "unavailable", "id": ID, "reason": "index_preparing"});
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
        "rift://symbol/local@cloud/rust/app/load",
        "rift://symbol/local:docs/typescript/page/beacon",
        "rift://symbol/local@cloud:docs/typescript/page/beacon",
        "rift://symbol/npm/npmjs.org/@types/node/typescript/node/buffer/Buffer",
        "rift://symbol/stdlib/cpython/python/builtins/len",
        "rift://symbol/cargo/packages.example%2Freleases/demo/rust/demo/run",
    ] {
        SymbolId::parse(id).expect("canonical fixture");
        assert!(validator.is_valid(&json!({"id": id})), "{id}");
    }
    for value in [
        json!({"id": "rift://symbol/rust/src/lib.rs/parse"}),
        json!({"id": ID, "max_results": 0}),
        json!({"id": ID, "max_results": 10001}),
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

const PACKAGE_ID: &str = "rift://symbol/cargo/crates.io/demo/rust/demo/run";

/// A found package symbol whose history lists three release edges, newest release first.
fn released_symbol() -> Value {
    let mut symbol = symbol();
    symbol["id"] = json!(PACKAGE_ID);
    symbol["name"] = json!("run");
    json!({
        "outcome": "found",
        "symbol": symbol,
        "revision": "rift://revision/cargo/crates.io/demo@1.1.0",
        "declarations": [],
        "pagination": {"page": 1, "pages": 0, "max_results": 10, "total": 0},
        "history": {
            "symbol": PACKAGE_ID,
            "edges": [
                {"edge": "removed_in_release", "revision": "rift://revision/cargo/crates.io/demo@2.0.0"},
                {
                    "edge": "changed_in_release",
                    "revision": "rift://revision/cargo/crates.io/demo@1.1.0",
                    "change": "signature_changed"
                },
                {"edge": "introduced_in_release", "revision": "rift://revision/cargo/crates.io/demo@1.0.0"}
            ],
            "complete": true
        }
    })
}

#[test]
fn release_history_round_trips_in_the_served_schema() {
    let requested = SymbolId::parse(PACKAGE_ID).expect("valid fixture");
    let found = released_symbol();
    let schema = serde_json::to_value(schemars::schema_for!(GetSymbolResult)).expect("schema");
    let validator = jsonschema::validator_for(&schema).expect("schema compiles");
    assert!(validator.is_valid(&found), "{found}");
    let result: GetSymbolResult = serde_json::from_value(found.clone()).expect("valid fixture");
    assert!(result.is_valid_for(&requested));
    assert_eq!(serde_json::to_value(&result).expect("valid fixture"), found);
    let GetSymbolResult::Found {
        history: Some(read),
        ..
    } = &result
    else {
        panic!("found answer carries its history");
    };
    let versions: Vec<_> = read
        .edges
        .iter()
        .filter_map(|edge| edge.release()?.release_version())
        .collect();
    assert_eq!(versions, ["2.0.0", "1.1.0", "1.0.0"]);
}

#[test]
fn release_history_answers_only_for_its_package_owner() {
    let requested = SymbolId::parse(PACKAGE_ID).expect("valid fixture");
    let altered = |member: &str, value: Value| {
        let mut altered = released_symbol();
        altered["history"][member] = value;
        serde_json::from_value::<GetSymbolResult>(altered)
            .map(|result| result.is_valid_for(&requested))
    };
    let other_owner = json!([{"edge": "introduced_in_release", "revision": "rift://revision/cargo/crates.io/other@1.0.0"}]);
    assert!(
        !altered("edges", other_owner).expect("valid fixture"),
        "a release of another owner"
    );
    let commit = json!([{"edge": "introduced_in", "commit": {
        "revision": COMMIT_REVISION,
        "path": "src/lib.rs",
        "timestamp": "2026-08-17T09:41:05+00:00",
        "author": {"name": "Alice", "email": "alice@example.com"}
    }}]);
    assert!(
        !altered("edges", commit).expect("valid fixture"),
        "a commit edge for a package"
    );
    assert!(
        !altered(
            "symbol",
            json!("rift://symbol/cargo/crates.io/demo/rust/demo/stop")
        )
        .expect("valid fixture"),
        "a history of another symbol"
    );
    assert!(
        !altered(
            "co_changes_truncated",
            json!({"commits_past_bound": 1, "symbols_past_bound": 0})
        )
        .expect("valid fixture"),
        "a package history counts no co-changes"
    );
    let commit_form = json!([{"edge": "introduced_in_release", "revision": COMMIT_REVISION}]);
    assert!(
        !altered("edges", commit_form).expect("valid fixture"),
        "a release edge naming a commit"
    );
    let commit_field = json!([{"edge": "introduced_in_release", "commit": COMMIT_REVISION}]);
    assert!(
        altered("edges", commit_field).is_err(),
        "a release edge carries `revision`"
    );
}

#[test]
fn a_project_history_carries_no_release_edge() {
    let project = SymbolId::parse(ID).expect("valid fixture");
    let mut local = released_symbol();
    local["symbol"]["id"] = json!(ID);
    local["revision"] = json!(COMMIT_REVISION);
    local["history"]["symbol"] = json!(ID);
    assert!(
        !serde_json::from_value::<GetSymbolResult>(local.clone())
            .expect("valid fixture")
            .is_valid_for(&project)
    );
    local["history"]["edges"] = json!([]);
    assert!(
        serde_json::from_value::<GetSymbolResult>(local)
            .expect("valid fixture")
            .is_valid_for(&project)
    );
}
