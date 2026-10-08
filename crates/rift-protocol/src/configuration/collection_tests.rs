use super::{
    ByteSize, ConfigurationViolation, LEXICAL_CONTENT_BYTES_DEFAULT, LEXICAL_CONTENT_BYTES_MAX,
    LEXICAL_CONTENT_BYTES_MIN, LEXICAL_DOCUMENTATION_BYTES_DEFAULT,
    LEXICAL_DOCUMENTATION_BYTES_MAX, LEXICAL_DOCUMENTATION_BYTES_MIN, LEXICAL_MATCHES_DEFAULT,
    LEXICAL_MATCHES_MAX, LEXICAL_MATCHES_MIN, SEARCH_RESULTS_DEFAULT, SEARCH_RESULTS_MAX,
    SEARCH_RESULTS_MIN, WorkspaceConfiguration,
};

type Setter = fn(&mut WorkspaceConfiguration, u64);

#[test]
fn test_collection_bounds_accept_edges_and_refuse_outside() {
    let cases: [(&str, Setter, u64, u64); 4] = [
        (
            "search.results",
            |configuration, value| configuration.search.results = value,
            SEARCH_RESULTS_MIN,
            SEARCH_RESULTS_MAX,
        ),
        (
            "search.lexical.max_matches",
            |configuration, value| configuration.search.lexical.max_matches = value,
            LEXICAL_MATCHES_MIN,
            LEXICAL_MATCHES_MAX,
        ),
        (
            "search.lexical.max_content",
            |configuration, value| {
                configuration.search.lexical.max_content = ByteSize::from_bytes(value);
            },
            LEXICAL_CONTENT_BYTES_MIN,
            LEXICAL_CONTENT_BYTES_MAX,
        ),
        (
            "search.lexical.max_documentation",
            |configuration, value| {
                configuration.search.lexical.max_documentation = ByteSize::from_bytes(value);
            },
            LEXICAL_DOCUMENTATION_BYTES_MIN,
            LEXICAL_DOCUMENTATION_BYTES_MAX,
        ),
    ];
    for (field, set, minimum, maximum) in cases {
        for value in [minimum, maximum] {
            let mut configuration = WorkspaceConfiguration::default();
            set(&mut configuration, value);
            assert_eq!(configuration.validate(), Ok(()), "{field} = {value}");
        }
        for value in [minimum - 1, maximum + 1] {
            let mut configuration = WorkspaceConfiguration::default();
            set(&mut configuration, value);
            let violation = configuration
                .validate()
                .expect_err("outside accepted range");
            assert!(
                matches!(violation, ConfigurationViolation::LimitOutOfRange { field: found, .. } if found == field),
                "{field} = {value}: {violation:?}"
            );
        }
    }
}

#[test]
fn test_collection_schema_ranges_match_runtime_bounds() {
    let schema =
        serde_json::to_value(schemars::schema_for!(WorkspaceConfiguration)).expect("schema");
    let search = &schema["$defs"]["SearchConfiguration"]["properties"];
    let lexical = &schema["$defs"]["LexicalSearchConfiguration"]["properties"];
    assert_eq!(search["results"]["default"], SEARCH_RESULTS_DEFAULT);
    assert_eq!(search["results"]["minimum"], SEARCH_RESULTS_MIN);
    assert_eq!(search["results"]["maximum"], SEARCH_RESULTS_MAX);
    assert_eq!(lexical["max_matches"]["default"], LEXICAL_MATCHES_DEFAULT);
    assert_eq!(lexical["max_matches"]["minimum"], LEXICAL_MATCHES_MIN);
    assert_eq!(lexical["max_matches"]["maximum"], LEXICAL_MATCHES_MAX);
    for (key, default, minimum, maximum) in [
        (
            "max_content",
            LEXICAL_CONTENT_BYTES_DEFAULT,
            LEXICAL_CONTENT_BYTES_MIN,
            LEXICAL_CONTENT_BYTES_MAX,
        ),
        (
            "max_documentation",
            LEXICAL_DOCUMENTATION_BYTES_DEFAULT,
            LEXICAL_DOCUMENTATION_BYTES_MIN,
            LEXICAL_DOCUMENTATION_BYTES_MAX,
        ),
    ] {
        assert_eq!(
            lexical[key]["default"],
            serde_json::json!(ByteSize::from_bytes(default))
        );
        assert_eq!(
            lexical[key]["rift:range"],
            serde_json::json!({"min": ByteSize::from_bytes(minimum), "max": ByteSize::from_bytes(maximum)})
        );
    }
}
