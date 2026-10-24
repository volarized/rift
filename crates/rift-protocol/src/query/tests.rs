use super::*;
use crate::configuration::{ByteSize, ConfigurationViolation};
use serde_json::json;

#[test]
fn query_numeric_schema_bounds_match_configuration_admission() {
    let schema =
        serde_json::to_value(schemars::schema_for!(QueryConfiguration)).expect("query schema");
    for (field, maximum) in [
        ("algebra_nodes", ALGEBRA_NODES_MAX),
        ("requests", REQUESTS_MAX),
        ("statements", STATEMENTS_MAX),
        ("results", RESULT_ITEMS_MAX),
        ("page_size", PAGE_ITEMS_MAX),
    ] {
        assert_eq!(schema["properties"][field]["minimum"], 1);
        assert_eq!(schema["properties"][field]["maximum"], maximum);
        for value in [0, 1, maximum, maximum + 1] {
            let mut wire =
                serde_json::to_value(QueryConfiguration::default()).expect("query default");
            wire[field] = json!(value);
            let configuration: QueryConfiguration =
                serde_json::from_value(wire).expect("configuration shape");
            assert_eq!(
                configuration.violation().is_some(),
                value == 0 || value > maximum,
                "{field}={value}"
            );
        }
    }
}

#[test]
fn query_byte_configuration_accepts_exact_bounds_and_refuses_one_over() {
    type Field = fn(&mut QueryConfiguration) -> &mut ByteSize;
    let cases: [(&str, u64, u64, Field); 4] = [
        (
            "statement_size",
            STATEMENT_BYTES_MIN,
            STATEMENT_BYTES_MAX,
            |value| &mut value.statement_size,
        ),
        ("fact_size", FACT_BYTES_MIN, FACT_BYTES_MAX, |value| {
            &mut value.fact_size
        }),
        ("result_size", RESULT_BYTES_MIN, RESULT_BYTES_MAX, |value| {
            &mut value.result_size
        }),
        (
            "page_limit",
            PAGE_BYTES_MIN,
            QUERY_PAGE_BYTES_MAX,
            |value| &mut value.page_limit,
        ),
    ];
    for (field, minimum, maximum, select) in cases {
        for bytes in [minimum - 1, minimum, maximum, maximum + 1] {
            let mut configuration = QueryConfiguration::default();
            *select(&mut configuration) = ByteSize::from_bytes(bytes);
            let violation = configuration.violation();
            assert_eq!(
                violation.is_some(),
                bytes < minimum || bytes > maximum,
                "{field}={bytes}"
            );
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
