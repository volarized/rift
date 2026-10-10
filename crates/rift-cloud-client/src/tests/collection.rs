use super::*;
use rift_core::acceptance::{ConfigurationEnvironment, accept_configuration};
use rift_protocol::configuration::{
    GLOBAL_REQUEST_BYTES_MAX, GLOBAL_RESPONSE_BYTES_MAX, GLOBAL_SOURCE_BYTES_MAX,
    WorkspaceConfiguration,
};

#[test]
fn test_collection_documentation_source_uses_smaller_configured_and_advertised_bound() {
    use serde_json::json;
    let mut capabilities: Capabilities =
        serde_json::from_str(&capabilities_json()).expect("capabilities");
    capabilities
        .supported_features
        .push("documentation_search".to_owned());
    capabilities.documentation_revision = Some("0123abcd".to_owned());
    let owner = json!({"source":{"kind":"package","unit":"rift://source/cargo/crates.io/demo@1.0.0/README.md"}});
    let mut value = search_page_json("demo", None, "analyzer-v1", "first");
    value["documentation_revision"] = json!("0123abcd");
    value["items"] = json!([{
        "target":"documentation","package":package_json("demo"),"source":"guide",
        "contributing_fields":["content","documentation"],
        "documentation":{
            "documentation_revision":"0123abcd",
            "block":{"identity":"1".repeat(64),"source":owner,"content_digest":"2".repeat(64),
                "range":{"start":0,"end":40},"line":1,"kind":"prose"},
            "source":{"identity":owner,"revision":"3".repeat(64),"content_digest":"4".repeat(64),
                "origin":{"location":"dependency","package":package_json("demo"),"source_kind":"authored"},
                "format":"markdown","media_type":"text/markdown","selection":"package_archive","byte_length":40}
        }
    }]);
    let documentation = value["items"][0]["documentation"].clone();
    let page: PackageSearchPage = serde_json::from_value(value).expect("documentation page");
    let mut request = search_request();
    request.target = Some(PackageSearchRequestTarget::Documentation);
    request.include = Some(vec!["source".to_owned()]);
    let document = "[global]\nmax_source = \"1b\"\n";
    let config = accepted_config(
        document,
        &ConfigurationEnvironment::default(),
        "https://example.test",
    );
    assert_eq!(
        validate_search_page(&request, &capabilities, &page, None, config.max_source),
        Err(ClientError::InvalidResponseField { field: "source" })
    );
    let environment = ConfigurationEnvironment::from_variables([("RIFT_GLOBAL_MAX_SOURCE", "5b")]);
    let config = accepted_config(document, &environment, "https://example.test");
    validate_search_page(&request, &capabilities, &page, None, config.max_source)
        .expect("exact configured documentation source bound");
    capabilities.bounds.source_bytes_max = 4;
    assert_eq!(
        validate_search_page(&request, &capabilities, &page, None, config.max_source),
        Err(ClientError::InvalidResponseField { field: "source" })
    );
    capabilities.bounds.source_bytes_max = i64::try_from(SOURCE_BYTES_MAX).expect("source bound");
    capabilities
        .supported_features
        .push("symbol_documentation".to_owned());
    let mut value = symbol_page_json("demo", None, "analyzer-v1", "first");
    value["documentation_revision"] = json!("0123abcd");
    value["items"][0]["documentation"] = json!({
        "documentation_revision":"0123abcd","truncated":false,
        "references":[{
            "reference":{"identity":"5".repeat(64),"block":"1".repeat(64),
                "target":symbol_json("demo","first")["id"],"evidence":"unique_name",
                "authored":"demo","range":{"start":1,"end":5}},
            "documentation":documentation,"excerpt":"guide"
        }]
    });
    let page: PackageSymbolPage = serde_json::from_value(value).expect("symbol context page");
    let mut request = symbol_request();
    request.include = Some(vec![PackageSymbolRequestInclude::Documentation]);
    assert_eq!(
        validate_symbol_page(&request, &capabilities, &page, None, 1),
        Err(ClientError::InvalidResponseField { field: "source" })
    );
    validate_symbol_page(&request, &capabilities, &page, None, config.max_source)
        .expect("exact configured documentation excerpt bound");
    capabilities.bounds.source_bytes_max = 4;
    assert_eq!(
        validate_symbol_page(&request, &capabilities, &page, None, config.max_source),
        Err(ClientError::InvalidResponseField { field: "source" })
    );
}

#[test]
fn test_collection_config_accepts_edges_and_refuses_outside_supported_bounds() {
    type Setter = fn(&mut Config, usize);
    for (field, maximum, set) in [
        (
            "max_request",
            usize::try_from(GLOBAL_REQUEST_BYTES_MAX).expect("request bound"),
            (|config, value| config.max_request = value) as Setter,
        ),
        (
            "max_response",
            usize::try_from(GLOBAL_RESPONSE_BYTES_MAX).expect("response bound"),
            (|config, value| config.max_response = value) as Setter,
        ),
        (
            "max_source",
            usize::try_from(GLOBAL_SOURCE_BYTES_MAX).expect("source bound"),
            (|config, value| config.max_source = value) as Setter,
        ),
    ] {
        for value in [1, maximum] {
            let mut config = Config::default();
            set(&mut config, value);
            assert_eq!(validate_config(&config), Ok(()), "{field}={value}");
        }
        for value in [0, maximum + 1] {
            let mut config = Config::default();
            set(&mut config, value);
            assert_eq!(
                validate_config(&config),
                Err(ConfigError::OutOfRange(field)),
                "{field}={value}"
            );
        }
    }

    let mut capabilities: Capabilities =
        serde_json::from_str(&capabilities_json()).expect("capabilities fixture");
    capabilities.bounds.request_body_bytes_max =
        i64::try_from(GLOBAL_REQUEST_BYTES_MAX).expect("request bound");
    capabilities.bounds.response_body_bytes_max =
        i64::try_from(GLOBAL_RESPONSE_BYTES_MAX).expect("response bound");
    capabilities.bounds.source_bytes_max =
        i64::try_from(GLOBAL_SOURCE_BYTES_MAX).expect("source bound");
    assert_eq!(validate_capabilities(&capabilities), Ok(()));
    capabilities.bounds.source_bytes_max += 1;
    assert_eq!(
        validate_capabilities(&capabilities),
        Err(ClientError::InvalidResponseField { field: "bounds" })
    );
}

fn accepted_config(
    document: &str,
    environment: &ConfigurationEnvironment,
    endpoint: &str,
) -> Config {
    let accepted = accept_configuration::<WorkspaceConfiguration>(Some(document), environment)
        .expect("accepted global collection configuration");
    accepted
        .configuration()
        .validate()
        .expect("validated global collection configuration");
    let mut config =
        Config::try_from(&accepted.configuration().global).expect("client configuration");
    config.endpoint = endpoint.to_owned();
    config
}

#[tokio::test]
async fn test_collection_config_and_environment_deliver_source_past_default_bound() {
    let source_bytes = SOURCE_BYTES_MAX + 1;
    let server = FixtureServer::start(FixtureMode::Operations(
        OperationFixture::CollectionBounds {
            source_bytes,
            source_bytes_max: 2 << 20,
        },
    ))
    .await
    .expect("fixture server");
    let mut search = search_request();
    search.include = Some(vec!["source".to_owned()]);
    let default = GlobalClient::new(server.config()).expect("default client");
    assert_eq!(
        default.search_packages(&search, 20, None).await,
        Err(ClientError::InvalidResponseField { field: "source" })
    );

    let document =
        "[global]\nmax_request = \"8mb\"\nmax_response = \"4mb\"\nmax_source = \"2mb\"\n";
    let config = accepted_config(
        document,
        &ConfigurationEnvironment::default(),
        &server.endpoint,
    );
    let client = GlobalClient::new(config).expect("configured client");
    let page = client
        .search_packages(&search, 20, None)
        .await
        .expect("configured source accepted");
    let candidate =
        PackageSearchCandidate::try_from(page.items.into_iter().next().expect("search item"))
            .expect("source conversion accepts supported size");
    assert_eq!(
        candidate.hit.source.as_ref().map(String::len),
        Some(source_bytes)
    );

    let environment = ConfigurationEnvironment::from_variables([
        ("RIFT_GLOBAL_MAX_REQUEST", "8mb"),
        ("RIFT_GLOBAL_MAX_RESPONSE", "4mb"),
        ("RIFT_GLOBAL_MAX_SOURCE", "2mb"),
    ]);
    let config = accepted_config(
        "[global]\nmax_source = \"1mb\"\n",
        &environment,
        &server.endpoint,
    );
    assert_eq!(config.max_request, 8 << 20);
    assert_eq!(config.max_response, 4 << 20);
    assert_eq!(config.max_source, 2 << 20);
    let client = GlobalClient::new(config).expect("environment client");
    let mut symbols = symbol_request();
    symbols.include = Some(vec![PackageSymbolRequestInclude::Source]);
    let page = client
        .list_package_symbols(&symbols, 20, None)
        .await
        .expect("configured symbol source");
    let candidate =
        PackageSymbolCandidate::try_from(page.items.into_iter().next().expect("symbol item"))
            .expect("source conversion accepts supported size");
    assert_eq!(
        candidate.hit.source.as_ref().map(String::len),
        Some(source_bytes)
    );

    let mut patterns = pattern::pattern_request();
    patterns.include = Some(vec!["source".to_owned()]);
    let page = client
        .search_package_patterns(&patterns, 20, None)
        .await
        .expect("configured pattern source");
    let candidate = PackagePatternMatch::try_from(page.items.first().expect("pattern item"))
        .expect("source conversion accepts supported size");
    assert_eq!(
        candidate.file.source.as_ref().map(String::len),
        Some(source_bytes)
    );
    assert_eq!(
        candidate
            .declaration
            .as_ref()
            .and_then(|hit| hit.source.as_ref())
            .map(String::len),
        Some(source_bytes)
    );
}

#[tokio::test]
async fn test_collection_operations_use_smaller_advertised_and_configured_bounds() {
    let server = FixtureServer::start(FixtureMode::Operations(
        OperationFixture::CollectionBounds {
            source_bytes: SOURCE_BYTES_MAX + 1,
            source_bytes_max: SOURCE_BYTES_MAX,
        },
    ))
    .await
    .expect("fixture server");
    let mut search = search_request();
    search.include = Some(vec!["source".to_owned()]);
    let mut config = server.config();
    config.max_source = 2 << 20;
    let client = GlobalClient::new(config).expect("configured client");
    assert_eq!(
        client.search_packages(&search, 20, None).await,
        Err(ClientError::InvalidResponseField { field: "source" })
    );

    let mut config = server.config();
    config.max_request = 1;
    let client = GlobalClient::new(config).expect("request bound client");
    assert!(matches!(
        client.search_packages(&search, 20, None).await,
        Err(ClientError::RequestBodyTooLarge { .. })
    ));
    let mut config = server.config();
    config.max_response = 4096;
    let client = GlobalClient::new(config).expect("response bound client");
    assert!(matches!(
        client.search_packages(&search, 20, None).await,
        Err(ClientError::ResponseBodyTooLarge { .. })
    ));
}

#[test]
fn test_collection_environment_refuses_outside_supported_bounds() {
    for (variable, value, field) in [
        ("RIFT_GLOBAL_MAX_REQUEST", "0b", "global.max_request"),
        (
            "RIFT_GLOBAL_MAX_RESPONSE",
            "1073741825b",
            "global.max_response",
        ),
        ("RIFT_GLOBAL_MAX_SOURCE", "67108865b", "global.max_source"),
    ] {
        let environment = ConfigurationEnvironment::from_variables([(variable, value)]);
        let accepted = accept_configuration::<WorkspaceConfiguration>(None, &environment)
            .expect("quantity shape accepted");
        assert!(matches!(accepted.configuration().validate(),
            Err(rift_protocol::configuration::ConfigurationViolation::LimitOutOfRange { field: refused, .. }) if refused == field));
    }
}
