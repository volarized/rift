use rift_protocol::configuration::WorkspaceConfiguration;

use crate::acceptance::{ConfigurationEnvironment, accept_configuration};
use crate::configuration::configuration_violation_error;

#[test]
fn saved_registration_table_passes_existing_configuration_acceptance() {
    let accepted = accept_configuration::<WorkspaceConfiguration>(
        Some("[mcp.extra]\ncloud = '../rift-cloud'\n"),
        &ConfigurationEnvironment::default(),
    )
    .expect("existing configuration admission accepts the table");
    accepted.configuration().validate().expect("valid mapping");
    assert_eq!(accepted.configuration().mcp.extra["cloud"], "../rift-cloud");
}

#[test]
fn invalid_registration_uses_registered_configuration_error_with_entry_context() {
    let accepted = accept_configuration::<WorkspaceConfiguration>(
        Some("[mcp.extra]\nlocal = '../app'\n"),
        &ConfigurationEnvironment::default(),
    )
    .expect("document shape accepted before value validation");
    let violation = accepted
        .configuration()
        .validate()
        .expect_err("reserved name");
    let error = configuration_violation_error(&violation);
    assert_eq!(error.slug().as_str(), "rift.core.configuration_invalid");
    let evidence = error.context().collect::<Vec<_>>();
    assert!(
        evidence
            .iter()
            .any(|(key, value)| *key == "field" && value == "mcp.extra")
    );
    assert!(
        evidence
            .iter()
            .any(|(key, value)| *key == "name" && value == "local")
    );
    assert!(
        evidence
            .iter()
            .any(|(key, value)| *key == "detail" && value == "registration name is not canonical")
    );
}
