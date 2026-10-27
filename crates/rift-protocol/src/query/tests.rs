use super::*;
use crate::configuration::{ByteSize, ConfigurationViolation};
use serde_json::json;

#[test]
fn optional_process_sizes_are_positive_and_keep_automatic_defaults() {
    let automatic = QueryConfiguration::default();
    let value = serde_json::to_value(&automatic).expect("query configuration");
    for field in [
        "input_size",
        "output_size",
        "material_size",
        "heap_size",
        "diagnostic_size",
    ] {
        assert!(value[field].is_null());
        let mut wire = value.clone();
        wire[field] = json!("0b");
        let configuration: QueryConfiguration = serde_json::from_value(wire).expect("byte shape");
        assert!(configuration.violation().is_some(), "{field}");
        let mut wire = value.clone();
        wire[field] = json!("1b");
        let configuration: QueryConfiguration = serde_json::from_value(wire).expect("byte shape");
        assert!(configuration.violation().is_none(), "{field}");
    }
}

#[test]
fn query_numeric_schema_bounds_match_configuration_admission() {
    let schema =
        serde_json::to_value(schemars::schema_for!(QueryConfiguration)).expect("query schema");
    for field in [
        "algebra_nodes",
        "requests",
        "statements",
        "results",
        "page_size",
    ] {
        assert_eq!(schema["properties"][field]["minimum"], 1);
        assert!(
            schema["properties"][field]["maximum"]
                .as_u64()
                .is_none_or(|maximum| maximum == u64::MAX)
        );
        for value in [0, 1, 1 << 40, u64::MAX] {
            let mut wire =
                serde_json::to_value(QueryConfiguration::default()).expect("query default");
            wire[field] = json!(value);
            let configuration: QueryConfiguration =
                serde_json::from_value(wire).expect("configuration shape");
            assert_eq!(
                configuration.violation().is_some(),
                value == 0,
                "{field}={value}"
            );
        }
    }
}

#[test]
fn query_byte_configuration_accepts_positive_policy_and_refuses_zero() {
    type Field = fn(&mut QueryConfiguration) -> &mut ByteSize;
    let cases: [(&str, Field); 4] = [
        ("statement_size", |value| &mut value.statement_size),
        ("fact_size", |value| &mut value.fact_size),
        ("result_size", |value| &mut value.result_size),
        ("page_limit", |value| &mut value.page_limit),
    ];
    for (field, select) in cases {
        for bytes in [0, 1, 1 << 40, u64::MAX] {
            let mut configuration = QueryConfiguration::default();
            *select(&mut configuration) = ByteSize::from_bytes(bytes);
            let violation = configuration.violation();
            assert_eq!(violation.is_some(), bytes == 0, "{field}={bytes}");
            if let Some(ConfigurationViolation::LimitOutOfRange {
                field: reported, ..
            }) = violation
            {
                assert_eq!(reported, format!("search.query.{field}"));
            }
        }
    }
}

#[test]
fn query_language_and_continuation_keep_strict_protocol_shapes() {
    let request: QueryParams =
        serde_json::from_value(json!({"query":"ASK {}"})).expect("default language");
    assert_eq!(request.language, QueryLanguage::Sparql);
    for request in [
        json!({"query":"ASK {}","language":"cypher"}),
        json!({"query":"ASK {}","scope":"local"}),
        json!({"query":"ASK {}","language":null}),
    ] {
        assert!(serde_json::from_value::<QueryParams>(request).is_err());
    }
    let page = QueryPagination {
        page: Pagination {
            page_index: 1,
            total_pages: 3,
        },
        next_query: Some(
            "SELECT ?x { SERVICE <urn:rift:result:claims.page?page=2> {} }".to_owned(),
        ),
    };
    let wire = serde_json::to_value(&page).expect("page wire");
    assert_eq!(wire["page_index"], 1);
    assert_eq!(wire["total_pages"], 3);
    assert!(wire.get("page").is_none());
    assert_eq!(
        serde_json::from_value::<QueryPagination>(wire).expect("strict page"),
        page
    );
    assert!(
        serde_json::from_value::<QueryPagination>(
            json!({"page_index":0,"total_pages":1,"unknown":true})
        )
        .is_err()
    );
}
