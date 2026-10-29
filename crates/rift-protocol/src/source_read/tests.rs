use super::*;
use serde_json::json;

const PROJECT: &str = "rift://source/project/src/lib.rs";
const RUNTIME: &str = "rift://source/stdlib/cpython@3.12.9/Lib/pathlib.py";

fn unit(value: &str) -> SourceUnitId {
    SourceUnitId::parse(value).expect("canonical source fixture")
}

fn view(value: &str) -> CapturedView {
    CapturedView {
        id: CapturedViewId::parse(value).expect("captured view fixture"),
        expires_at: "2026-10-10T03:00:00Z".to_owned(),
    }
}

fn request() -> FindDeclarationsParams {
    FindDeclarationsParams {
        position_encoding: PositionEncoding::Utf8,
        positions: vec![position(PROJECT)],
        view: None,
    }
}

fn position(value: &str) -> SourcePosition {
    SourcePosition {
        unit: unit(value),
        line: 0,
        character: 0,
    }
}

#[test]
fn position_selectors_accept_canonical_units_and_refuse_legacy_owners() {
    let mut request = request();
    assert!(request.is_valid());
    request.positions = vec![position(RUNTIME)];
    request.view = Some(view("payload.signature").id);
    assert!(request.is_valid());
    assert!(
        serde_json::from_value::<SourcePosition>(
            json!({"unit":"rift://source/stdlib/python/3.12.9/pathlib.py","line":0,"character":0})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<SourcePosition>(
            json!({"unit":PROJECT,"line":0,"character":0,"name":"beacon"})
        )
        .is_err()
    );
}

#[test]
fn position_bounds_match_schema_and_batch_order() {
    let mut request = FindDeclarationsParams {
        position_encoding: PositionEncoding::Utf16,
        positions: vec![position(PROJECT)],
        view: None,
    };
    request.positions[0].line = SOURCE_POSITION_MAX;
    request.positions[0].character = SOURCE_POSITION_MAX;
    assert!(request.is_valid());
    let schema = schemars::schema_for!(FindDeclarationsParams);
    let validator = jsonschema::validator_for(&serde_json::to_value(schema).expect("schema"))
        .expect("validator");
    assert!(validator.is_valid(&serde_json::to_value(&request).expect("request")));
    request.positions[0].line += 1;
    assert!(!request.is_valid());
    assert!(!validator.is_valid(&serde_json::to_value(&request).expect("request")));
    request.positions = (0..SOURCE_POSITIONS_MAX)
        .map(|index| SourcePosition {
            character: u64::try_from(index).expect("bounded coordinate"),
            ..position(PROJECT)
        })
        .collect();
    assert!(request.is_valid());
    let mut duplicate = request.clone();
    duplicate.positions[1] = duplicate.positions[0].clone();
    assert!(!duplicate.is_valid());
    assert!(!validator.is_valid(&serde_json::to_value(&duplicate).expect("duplicate request")));
    request.positions.push(position(PROJECT));
    assert!(!request.is_valid());
    request.positions.clear();
    assert!(!request.is_valid());
}

#[test]
fn position_results_share_view_without_conflating_physical_and_logical_owner() {
    let request = FindDeclarationsParams {
        position_encoding: PositionEncoding::Utf8,
        positions: vec![
            position("rift://source/pypi/pypi.org/types-pathlib@1.0.0/pathlib.pyi"),
            position(PROJECT),
        ],
        view: None,
    };
    let mut result = FindDeclarationsResult {
        results: vec![
            DeclarationPositionResult::Found {
                position: request.positions[0].clone(),
                id: SymbolId::parse("rift://symbol/stdlib/cpython@3.12.9/python/pathlib/Path")
                    .expect("runtime symbol"),
                kind: ExactKind("class".to_owned()),
                view: view("payload.signature"),
            },
            DeclarationPositionResult::Missing {
                position: request.positions[1].clone(),
                view: view("payload.signature"),
            },
        ],
        warnings: Vec::new(),
    };
    assert!(result.is_valid_for(&request));
    let DeclarationPositionResult::Found { kind, .. } = &mut result.results[0] else {
        panic!("found fixture")
    };
    *kind = ExactKind("9function".to_owned());
    assert!(!result.is_valid_for(&request));
    let DeclarationPositionResult::Found { kind, .. } = &mut result.results[0] else {
        panic!("found fixture")
    };
    *kind = ExactKind("class".to_owned());
    result.results.swap(0, 1);
    assert!(!result.is_valid_for(&request));
    result.results.swap(0, 1);
    let DeclarationPositionResult::Missing { view: selected, .. } = &mut result.results[1] else {
        panic!("missing fixture")
    };
    *selected = view("foreign.signature");
    assert!(!result.is_valid_for(&request));
    let legacy = json!({"outcome":"found","position":{"unit":PROJECT,"line":0,"character":0},"id":"rift://symbol/rust/src/lib.rs/beacon","kind":"function","view":view("payload.signature")});
    assert!(serde_json::from_value::<DeclarationPositionResult>(legacy).is_err());
}
