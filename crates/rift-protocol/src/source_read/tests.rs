use super::*;
use serde_json::json;

const PACKAGE: &str = "rift://source/cargo/crates.io/demo/src/lib.rs";
const PACKAGE_RELEASE: &str = "rift://revision/cargo/crates.io/demo@1.0.0";
const PROJECT: &str = "rift://source/project/src/lib.rs";
const RUNTIME: &str = "rift://source/stdlib/cpython/Lib/pathlib.py";
const RUNTIME_RELEASE: &str = "rift://revision/stdlib/cpython@3.12.9";

fn unit(value: &str) -> SourceUnitId {
    SourceUnitId::parse(value).expect("canonical source fixture")
}

fn revision(value: &str) -> Revision {
    Revision::parse(value).expect("release fixture")
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
        positions: vec![position(PACKAGE, PACKAGE_RELEASE)],
        view: None,
    }
}

fn position(value: &str, release: &str) -> SourcePosition {
    SourcePosition {
        unit: unit(value),
        revision: Some(revision(release)),
        line: 0,
        character: 0,
    }
}

#[test]
fn position_selectors_accept_released_units_and_refuse_legacy_owners() {
    let mut request = request();
    assert!(request.is_valid());
    request.positions = vec![position(RUNTIME, RUNTIME_RELEASE)];
    request.view = Some(view("payload.signature").id);
    assert!(request.is_valid());
    for (unit, release) in [
        (PROJECT, PACKAGE_RELEASE),
        (RUNTIME, PACKAGE_RELEASE),
        (PACKAGE, "rift://revision/cargo/crates.io/other@1.0.0"),
    ] {
        request.positions = vec![position(unit, release)];
        assert!(
            !request.is_valid(),
            "a position names a release of its unit's owner: {unit} at {release}"
        );
    }
    request.positions = vec![SourcePosition {
        revision: None,
        ..position(PACKAGE, PACKAGE_RELEASE)
    }];
    assert!(!request.is_valid(), "a released unit names its release");
    request.positions = vec![SourcePosition {
        revision: None,
        ..position(PROJECT, PACKAGE_RELEASE)
    }];
    assert!(request.is_valid(), "a unit no release owns names none");
    for refused in [
        json!({"unit":"rift://source/stdlib/cpython@3.12.9/pathlib.py","revision":RUNTIME_RELEASE,"line":0,"character":0}),
        json!({"unit":PACKAGE,"revision":PACKAGE_RELEASE,"line":0,"character":0,"name":"beacon"}),
    ] {
        assert!(
            serde_json::from_value::<SourcePosition>(refused.clone()).is_err(),
            "{refused}"
        );
    }
}

#[test]
fn position_bounds_match_schema_and_batch_order() {
    let mut request = FindDeclarationsParams {
        position_encoding: PositionEncoding::Utf16,
        positions: vec![position(PACKAGE, PACKAGE_RELEASE)],
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
            ..position(PACKAGE, PACKAGE_RELEASE)
        })
        .collect();
    assert!(request.is_valid());
    let mut duplicate = request.clone();
    duplicate.positions[1] = duplicate.positions[0].clone();
    assert!(!duplicate.is_valid());
    assert!(!validator.is_valid(&serde_json::to_value(&duplicate).expect("duplicate request")));
    request.positions.push(position(PACKAGE, PACKAGE_RELEASE));
    assert!(!request.is_valid());
    request.positions.clear();
    assert!(!request.is_valid());
}

#[test]
fn position_results_share_view_without_conflating_physical_and_logical_owner() {
    let request = FindDeclarationsParams {
        position_encoding: PositionEncoding::Utf8,
        positions: vec![
            position(
                "rift://source/pypi/pypi.org/types-pathlib/pathlib.pyi",
                "rift://revision/pypi/pypi.org/types-pathlib@1.0.0",
            ),
            position(PACKAGE, PACKAGE_RELEASE),
        ],
        view: None,
    };
    let mut result = FindDeclarationsResult {
        results: vec![
            DeclarationPositionResult::Found {
                position: request.positions[0].clone(),
                id: SymbolId::parse("rift://symbol/stdlib/cpython/python/pathlib/Path")
                    .expect("runtime symbol"),
                kind: ExactKind("class".to_owned()),
                revision: revision(RUNTIME_RELEASE),
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
    let found = |result: &mut FindDeclarationsResult, kind: &str, release: &str| {
        let DeclarationPositionResult::Found {
            kind: found_kind,
            revision: found_revision,
            ..
        } = &mut result.results[0]
        else {
            panic!("found fixture")
        };
        *found_kind = ExactKind(kind.to_owned());
        *found_revision = revision(release);
    };
    found(&mut result, "9function", RUNTIME_RELEASE);
    assert!(!result.is_valid_for(&request));
    found(
        &mut result,
        "class",
        "rift://revision/pypi/pypi.org/types-pathlib@1.0.0",
    );
    assert!(
        !result.is_valid_for(&request),
        "the answered revision is a release of the declaration's own owner"
    );
    found(&mut result, "class", RUNTIME_RELEASE);
    assert!(result.is_valid_for(&request));
    result.results.swap(0, 1);
    assert!(!result.is_valid_for(&request));
    result.results.swap(0, 1);
    let DeclarationPositionResult::Missing { view: selected, .. } = &mut result.results[1] else {
        panic!("missing fixture")
    };
    *selected = view("foreign.signature");
    assert!(!result.is_valid_for(&request));
    let legacy = json!({"outcome":"found","position":{"unit":PACKAGE,"revision":PACKAGE_RELEASE,"line":0,"character":0},"id":"rift://symbol/rust/src/lib.rs/beacon","kind":"function","view":view("payload.signature")});
    assert!(serde_json::from_value::<DeclarationPositionResult>(legacy).is_err());
}
