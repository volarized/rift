use super::*;
use serde_json::{Value, json};

const RANGE: &str = "~5.7.2";

fn exact_entry(version: &str) -> PackageContextEntry {
    PackageContextEntry {
        availability: PackageAvailability::Canonical,
        manager: "cargo".to_owned(),
        name: "demo".to_owned(),
        requirement: None,
        version: Some(version.to_owned()),
    }
}

fn requirement_entry() -> PackageContextEntry {
    PackageContextEntry {
        availability: PackageAvailability::Canonical,
        manager: "cargo".to_owned(),
        name: "demo".to_owned(),
        requirement: Some(RANGE.to_owned()),
        version: None,
    }
}

fn served(version: &str) -> PackageIdentity {
    PackageIdentity {
        manager: "cargo".to_owned(),
        name: "demo".to_owned(),
        version: version.to_owned(),
    }
}

fn entry_json(entry: &PackageContextEntry) -> Value {
    serde_json::to_value(entry).expect("context entry JSON")
}

/// A `requirement_unsatisfied` warning whose `detail` names `selector` answered by `version`.
fn warning_json(selector: &str, version: &str) -> Value {
    json!({"code": "requirement_unsatisfied", "detail": format!("cargo/demo {selector} answered by {version}")})
}

fn resolution(resolved: &Value, available: &Value, warnings: Option<Value>) -> Value {
    let mut body = json!({
        "available_exact": available,
        "resolved_requirements": resolved,
        "missing_exact": [],
        "missing_requirements": []
    });
    if let Some(warnings) = warnings {
        body["warnings"] = warnings;
    }
    body
}

/// The resolution bodies of the substitution fixtures; `None` for every other mode.
pub(super) fn substitution_response(mode: &OperationFixture) -> Option<Value> {
    let resolved = |entry: &PackageContextEntry, version: &str| json!([{"entry": entry_json(entry), "package": served(version)}]);
    let exact = exact_entry("1.0.3");
    let body = match mode {
        OperationFixture::NamedSubstitution => {
            resolution(&resolved(&exact, "1.0.2"), &json!([]), None)
        }
        OperationFixture::UnnamedSubstitution => {
            resolution(&json!([]), &json!([served("1.0.2")]), None)
        }
        OperationFixture::SubstitutionAtRequestedVersion => {
            resolution(&resolved(&exact, "1.0.3"), &json!([]), None)
        }
        OperationFixture::RequirementOutsideRange => {
            let requirement = requirement_entry();
            let warnings = json!([warning_json(RANGE, "5.9.3")]);
            resolution(&resolved(&requirement, "5.9.3"), &json!([]), Some(warnings))
        }
        _ => return None,
    };
    Some(body)
}

fn decoded(body: Value) -> PackageResolutionResponse {
    serde_json::from_value(body).expect("resolution fixture")
}

/// An exact entry answered at the nearest collected release names the served version beside
/// the requested entry, and the client hands the substitution to its caller.
#[tokio::test]
async fn test_fixture_resolution_accepts_a_named_substitution() {
    let (_server, client) = operation_client(OperationFixture::NamedSubstitution).await;
    let request = PackageResolutionRequest {
        entries: vec![exact_entry("1.0.3")],
    };
    let resolution = client
        .resolve_package_context(&request)
        .await
        .unwrap_or_else(|error| panic!("named substitution: {error:?}"));
    assert_eq!(
        resolution.substitutions(),
        [Substitution {
            requested: exact_entry("1.0.3"),
            served: served("1.0.2"),
        }]
    );

    let (_server, client) = operation_client(OperationFixture::Valid).await;
    let resolution = client
        .resolve_package_context(&resolution_request())
        .await
        .unwrap_or_else(|error| panic!("requirement resolution: {error:?}"));
    assert!(resolution.substitutions().is_empty());
}

#[tokio::test]
async fn test_fixture_resolution_refuses_an_unnamed_substitution() {
    let request = PackageResolutionRequest {
        entries: vec![exact_entry("1.0.3")],
    };
    let cases = [
        (
            OperationFixture::UnnamedSubstitution,
            "resolution_accounting",
        ),
        (
            OperationFixture::SubstitutionAtRequestedVersion,
            "resolved_requirement",
        ),
    ];
    for (mode, field) in cases {
        let (_server, client) = operation_client(mode).await;
        assert_eq!(
            client.resolve_package_context(&request).await,
            Err(ClientError::InvalidResponseField { field })
        );
    }
}

/// A requirement answered outside its range carries `requirement_unsatisfied`, and the client
/// hands it to its caller as a substitution beside the exact ones.
#[tokio::test]
async fn test_fixture_resolution_names_a_requirement_answered_outside_its_range() {
    let (_server, client) = operation_client(OperationFixture::RequirementOutsideRange).await;
    let request = PackageResolutionRequest {
        entries: vec![requirement_entry()],
    };
    let resolution = client
        .resolve_package_context(&request)
        .await
        .unwrap_or_else(|error| panic!("requirement outside its range: {error:?}"));
    assert_eq!(
        resolution.substitutions(),
        [Substitution {
            requested: requirement_entry(),
            served: served("5.9.3"),
        }]
    );
}

#[test]
fn test_a_requirement_resolved_in_its_range_names_no_substitution() {
    let requirement = requirement_entry();
    let resolved = json!([{"entry": entry_json(&requirement), "package": served("5.7.9")}]);
    let body = resolution(&resolved, &json!([]), None);
    let request = PackageResolutionRequest {
        entries: vec![requirement],
    };
    let response = decoded(body);
    assert_eq!(validate_resolution_response(&request, &response), Ok(()));
    assert!(response.substitutions().is_empty());
}

#[test]
fn test_resolution_refuses_a_warning_naming_no_answered_requirement() {
    let requirement = requirement_entry();
    let exact = exact_entry("1.0.3");
    let request = PackageResolutionRequest {
        entries: vec![requirement.clone(), exact.clone()],
    };
    let resolved = json!([
        {"entry": entry_json(&requirement), "package": served("5.9.3")},
        {"entry": entry_json(&exact), "package": served("1.0.2")}
    ]);
    let cases = [
        (
            "another served version",
            json!([warning_json(RANGE, "5.9.2")]),
        ),
        ("an exact entry", json!([warning_json("1.0.3", "1.0.2")])),
        (
            "the same requirement twice",
            json!([warning_json(RANGE, "5.9.3"), warning_json(RANGE, "5.9.3")]),
        ),
        ("no detail", json!([{"code": "requirement_unsatisfied"}])),
    ];
    for (case, warnings) in cases {
        let response = decoded(resolution(&resolved, &json!([]), Some(warnings)));
        assert_eq!(
            validate_resolution_response(&request, &response),
            Err(ClientError::InvalidResponseField {
                field: "resolution_warning"
            }),
            "{case}"
        );
    }

    let future = json!([{"code": "future_code", "detail": "cargo/demo 1.0.3 answered by 1.0.2"}]);
    let response = decoded(resolution(&resolved, &json!([]), Some(future)));
    assert_eq!(validate_resolution_response(&request, &response), Ok(()));
    assert_eq!(
        response
            .warnings
            .as_deref()
            .map(|warnings| &warnings[0].code),
        Some(&WarningCode::Unknown)
    );
    assert_eq!(
        response.substitutions(),
        [Substitution {
            requested: exact,
            served: served("1.0.2"),
        }],
        "an unknown code names no substitution"
    );
}

#[test]
fn test_resolution_refuses_warnings_past_the_entry_bound() {
    let requirement = requirement_entry();
    let request = PackageResolutionRequest {
        entries: vec![requirement.clone()],
    };
    let resolved = json!([{"entry": entry_json(&requirement), "package": served("5.9.3")}]);
    let unknown = json!({"code": "unknown"});
    let past_the_bound = [
        Value::Array(vec![unknown; DEPENDENCY_ENTRIES_MAX + 1]),
        json!([{"code": "unknown", "detail": ""}]),
        json!([{"code": "unknown", "detail": "x".repeat(WARNING_DETAIL_BYTES_MAX + 1)}]),
    ];
    for warnings in past_the_bound {
        let response = decoded(resolution(&resolved, &json!([]), Some(warnings)));
        assert_eq!(
            validate_resolution_response(&request, &response),
            Err(ClientError::InvalidResponseField { field: "warnings" })
        );
    }
    let at_the_bound = json!([{"code": "unknown", "detail": "x".repeat(WARNING_DETAIL_BYTES_MAX)}]);
    let response = decoded(resolution(&resolved, &json!([]), Some(at_the_bound)));
    assert_eq!(validate_resolution_response(&request, &response), Ok(()));
}
