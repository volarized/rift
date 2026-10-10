use std::collections::BTreeMap;

use serde_json::json;

use super::{McpConfiguration, registration_bytes};
use crate::configuration::{
    CONFIGURATION_FILE_BYTES_MAX, ConfigurationViolation, WorkspaceConfiguration,
};

#[test]
fn saved_registrations_keep_paths_and_project_configuration_separate() {
    let value: WorkspaceConfiguration = serde_json::from_value(json!({
        "mcp": {"extra": {"cloud": "../rift-cloud", "app": "~/projects/app"}}
    }))
    .expect("saved registrations");
    value.validate().expect("accepted configuration");
    assert_eq!(value.mcp.extra["cloud"], "../rift-cloud");
    assert_eq!(value.mcp.extra["app"], "~/projects/app");
    assert_eq!(value.languages, WorkspaceConfiguration::default().languages);
    assert_eq!(value.lsp, WorkspaceConfiguration::default().lsp);
    assert!(WorkspaceConfiguration::default().mcp.extra.is_empty());
}

#[test]
fn registration_schema_and_runtime_refuse_invalid_names_and_path_forms() {
    let schema = serde_json::to_value(schemars::schema_for!(McpConfiguration)).expect("schema");
    let validator = jsonschema::validator_for(&schema).expect("registration schema");
    for (name, path, accepted) in [
        ("cloud", "../rift-cloud", true),
        ("cloud_2", "/projects/my app=source", true),
        ("app-test", "C:\\projects\\app", true),
        ("app", "~", true),
        ("app", "~/projects/app", true),
        ("local", "../app", false),
        ("global", "../app", false),
        ("all", "../app", false),
        ("Cloud", "../app", false),
        ("2app", "../app", false),
        ("cloud/app", "../app", false),
        ("", "../app", false),
        ("app", "", false),
        ("app", "~person/app", false),
        ("app", "../app\0", false),
        ("app", "../app\u{85}", false),
    ] {
        let value = json!({"extra": {name: path}});
        assert_eq!(
            validator.is_valid(&value),
            accepted,
            "schema: {name} {path:?}"
        );
        let parsed: McpConfiguration = serde_json::from_value(value).expect("entry shape");
        assert_eq!(
            parsed.validate().is_ok(),
            accepted,
            "runtime: {name} {path:?}"
        );
    }
}

#[test]
fn aggregate_registration_budget_counts_utf8_bytes_and_all_entries() {
    let budget = usize::try_from(CONFIGURATION_FILE_BYTES_MAX).expect("budget fits usize");
    let mut path = "é".repeat((budget - "cloud".len()) / 2);
    path.push('x');
    let mut value = McpConfiguration {
        extra: BTreeMap::from([("cloud".to_owned(), path)]),
    };
    assert_eq!(value.extra["cloud"].len() + "cloud".len(), budget);
    value.validate().expect("exact aggregate boundary");
    value.extra.insert("app".to_owned(), ".".to_owned());
    assert_eq!(
        value.validate(),
        Err(ConfigurationViolation::LimitOutOfRange {
            field: "mcp.extra",
            value: CONFIGURATION_FILE_BYTES_MAX + 4,
            min: 0,
            max: CONFIGURATION_FILE_BYTES_MAX,
        })
    );
}

#[test]
fn registration_byte_accounting_refuses_overflow_before_root_resolution() {
    for lengths in [[(u64::MAX, 1), (0, 0)], [(u64::MAX - 1, 0), (1, 1)]] {
        assert!(matches!(
            registration_bytes(lengths),
            Err(ConfigurationViolation::LimitOutOfRange {
                field: "mcp.extra",
                ..
            })
        ));
    }
    assert_eq!(registration_bytes([(0, 0)]), Ok(0));
}

#[test]
fn registration_model_refuses_unknown_fields_and_nonstring_paths() {
    for value in [
        json!({"extra": {"cloud": 4}}),
        json!({"extra": {}, "projects": {}}),
    ] {
        assert!(serde_json::from_value::<McpConfiguration>(value).is_err());
    }
}

#[test]
fn accepted_registration_names_fit_the_canonical_local_scope() {
    let schema = serde_json::to_value(schemars::schema_for!(McpConfiguration)).expect("schema");
    let validator = jsonschema::validator_for(&schema).expect("registration schema");
    let boundary = crate::identity::SYMBOL_ID_BYTES_MAX - "local@".len();
    for (length, accepted) in [(boundary, true), (boundary + 1, false)] {
        let name = "a".repeat(length);
        let scope = format!("local@{name}");
        let value = json!({"extra": {name: "."}});
        let configuration: McpConfiguration =
            serde_json::from_value(value.clone()).expect("bounded registration shape");
        assert_eq!(crate::identity::parse_local_scope(&scope).is_ok(), accepted);
        assert_eq!(validator.is_valid(&value), accepted);
        assert_eq!(configuration.validate().is_ok(), accepted);
        if !accepted {
            assert!(matches!(
                configuration.validate(),
                Err(ConfigurationViolation::McpRegistrationInvalid {
                    field: "mcp.extra",
                    ..
                })
            ));
        }
    }
}
