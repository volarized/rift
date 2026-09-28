use super::*;
use crate::pattern::{
    PatternPageCheck, validate_pattern_request, validate_pattern_request_for_capabilities,
};
use rift_protocol::read::SEARCH_PATTERN_CHARS_MAX;
use serde_json::{Value, json};

const UNIT: &str = "rift://source/cargo/demo@1.0.0/src/first.rs";

/// One edit to a pattern page fixture, beside the field the client names when it refuses it.
type PageEdit = (&'static str, fn(&mut Value));

/// Size of the file every fixture match sits in.
const FILE_SIZE: u64 = 64;

fn pattern_hit_json(unit: &str, start: u64) -> Value {
    json!({
        "package": package_json("demo"), "unit": unit,
        "range": {"start": start, "end": start + 4}, "line": 2, "size": FILE_SIZE,
        "declaration": {"symbol": symbol_json("demo", "first"), "range": {"start": 0, "end": 40}, "line": 1}
    })
}

/// The fixture's pattern page: one match, then a second page behind the `next` cursor.
pub(super) fn pattern_page_json(cursor: Option<&str>) -> Value {
    let (start, next) = if cursor.is_none() {
        (10, json!("next"))
    } else {
        (20, Value::Null)
    };
    json!({
        "items": [pattern_hit_json(UNIT, start)], "next_cursor": next, "warnings": [],
        "publication_format": "rift-package-index-v2",
        "analyzer_revision": "analyzer-v1", "corpus_revision": "corpus-v1"
    })
}

fn pattern_request() -> PackagePatternRequest {
    PackagePatternRequest {
        pattern: r"fn\s+demo".to_owned(),
        packages: vec![package_request()],
        include: None,
    }
}

fn pattern_capabilities() -> Capabilities {
    let mut capabilities: Capabilities =
        serde_json::from_str(&capabilities_json()).expect("capabilities fixture");
    capabilities.supported_features.push("patterns".to_owned());
    capabilities
}

fn check(
    request: &PackagePatternRequest,
    capabilities: &Capabilities,
    page: Value,
    cursor: Option<&str>,
) -> Result<(), ClientError> {
    let page: PackagePatternPage = serde_json::from_value(page).expect("pattern page fixture");
    PatternPageCheck {
        request,
        capabilities,
        limit: 20,
        cursor,
    }
    .validate(&page)
}

#[tokio::test]
async fn test_fixture_pattern_pages_carry_matches_and_follow_the_callers_cursor() {
    let (server, client) = operation_client(OperationFixture::Patterns).await;
    let page = client
        .search_package_patterns(&pattern_request(), 20, None)
        .await
        .unwrap_or_else(|error| panic!("pattern page: {error:?}"));
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.next_cursor.as_deref(), Some("next"));
    let declaration = page.items[0]
        .declaration
        .as_ref()
        .expect("the match names its declaration");
    assert_eq!(declaration.symbol.name, "demo");
    assert_eq!(
        server.state.last_path.lock().await.as_deref(),
        Some("/rift/rest/v1/patterns")
    );
    assert_eq!(
        server.state.last_query.lock().await.as_deref(),
        Some("limit=20")
    );
    let body = server
        .state
        .request_log
        .lock()
        .await
        .last()
        .map(|request| request.body.clone())
        .expect("the pattern request");
    assert_eq!(
        serde_json::from_slice::<Value>(&body).ok(),
        serde_json::to_value(pattern_request()).ok()
    );

    let page = client
        .search_package_patterns(&pattern_request(), 20, Some("next"))
        .await
        .unwrap_or_else(|error| panic!("second pattern page: {error:?}"));
    assert_eq!(page.next_cursor, None);
    assert_eq!(
        server.state.last_query.lock().await.as_deref(),
        Some("limit=20&cursor=next")
    );
}

/// A pattern page is asked for at the smaller of the caller's limit and the advertised
/// `page_limit_max`, as the paged reads are.
#[tokio::test]
async fn test_fixture_pattern_pages_ask_for_the_advertised_page_limit() {
    let (server, client) = operation_client(OperationFixture::Patterns).await;
    let page = client
        .search_package_patterns(&pattern_request(), PAGE_LIMIT_MAX, None)
        .await;
    assert!(page.is_ok(), "{page:?}");
    assert_eq!(
        server.state.last_query.lock().await.as_deref(),
        Some("limit=200")
    );
}

#[tokio::test]
async fn test_fixture_pattern_search_requires_the_advertised_capability() {
    let (server, client) = operation_client(OperationFixture::Valid).await;
    assert_eq!(
        client
            .search_package_patterns(&pattern_request(), 20, None)
            .await,
        Err(ClientError::FeatureUnavailable {
            feature: "patterns"
        })
    );
    assert_eq!(
        server.state.last_path.lock().await.as_deref(),
        Some("/rift/rest/v1/capabilities"),
        "the refusal sends no pattern request"
    );

    let client = GlobalClient::new(Config {
        enabled: false,
        ..Config::default()
    })
    .expect("disabled client");
    assert_eq!(
        client
            .search_package_patterns(&pattern_request(), 20, None)
            .await,
        Err(ClientError::Disabled)
    );
}

#[tokio::test]
async fn test_fixture_pattern_failure_marks_the_endpoint_unavailable() {
    let (_server, client) = operation_client(OperationFixture::Patterns).await;
    let mut request = pattern_request();
    request.packages[0].name = "other".to_owned();
    assert_eq!(
        client.search_package_patterns(&request, 20, None).await,
        Err(ClientError::InvalidResponseField { field: "package" })
    );
    assert_eq!(
        client
            .search_package_patterns(&pattern_request(), 20, None)
            .await,
        Err(ClientError::InvalidResponseField { field: "package" }),
        "a refused page marks the endpoint unavailable, and a later request answers its failure"
    );
}

/// The contract's `maxLength` counts characters, so a pattern of that many two-byte
/// characters is accepted although it holds twice as many bytes, and one more refuses.
#[test]
fn test_the_pattern_bound_counts_characters_as_the_contract_does() {
    let mut request = pattern_request();
    request.pattern = "\u{e9}".repeat(SEARCH_PATTERN_CHARS_MAX);
    assert_eq!(request.pattern.len(), 2 * SEARCH_PATTERN_CHARS_MAX);
    assert_eq!(validate_pattern_request(&request), Ok(()));

    request.pattern.push('\u{e9}');
    assert_eq!(
        validate_pattern_request(&request),
        Err(ClientError::InvalidRequest { field: "pattern" })
    );
}

#[test]
fn test_pattern_requests_refuse_bounds_before_transport() {
    assert_eq!(validate_pattern_request(&pattern_request()), Ok(()));
    let cases = [
        (String::new(), "pattern"),
        ("x".repeat(SEARCH_PATTERN_CHARS_MAX + 1), "pattern"),
    ];
    for (pattern, field) in cases {
        let mut request = pattern_request();
        request.pattern = pattern;
        assert_eq!(
            validate_pattern_request(&request),
            Err(ClientError::InvalidRequest { field })
        );
    }
    let mut request = pattern_request();
    request.pattern = "x".repeat(SEARCH_PATTERN_CHARS_MAX);
    assert_eq!(validate_pattern_request(&request), Ok(()));

    for include in [
        vec!["score".to_owned()],
        vec!["source".to_owned(), "source".to_owned()],
    ] {
        let mut request = pattern_request();
        request.include = Some(include);
        assert_eq!(
            validate_pattern_request(&request),
            Err(ClientError::InvalidRequest { field: "include" })
        );
    }
    let mut request = pattern_request();
    request.packages.push(package_request());
    assert_eq!(
        validate_pattern_request(&request),
        Err(ClientError::InvalidRequest {
            field: "duplicate_package"
        })
    );
    let capabilities = pattern_capabilities();
    assert_eq!(
        validate_pattern_request_for_capabilities(&pattern_request(), 0, None, &capabilities),
        Err(ClientError::InvalidRequest { field: "limit" })
    );
}

#[test]
fn test_pattern_pages_refuse_matches_breaking_the_contract() {
    let request = pattern_request();
    let capabilities = pattern_capabilities();
    let base = || pattern_page_json(None);
    assert_eq!(check(&request, &capabilities, base(), None), Ok(()));

    let mut without_declaration = base();
    without_declaration["items"][0]
        .as_object_mut()
        .expect("hit object")
        .remove("declaration");
    assert_eq!(
        check(&request, &capabilities, without_declaration, None),
        Ok(())
    );

    let mutations: [PageEdit; 10] = [
        ("package", |page| {
            page["items"][0]["package"]["name"] = json!("other");
        }),
        ("location", |page| {
            page["items"][0]["line"] = json!(0);
        }),
        ("location", |page| {
            page["items"][0]["range"] = json!({"start": 9, "end": 8});
        }),
        ("location", |page| {
            page["items"][0]["size"] = json!(13);
        }),
        ("source", |page| {
            page["items"][0]["declaration"]["source"] = json!("fn demo");
        }),
        ("source", |page| {
            page["items"][0]["source"] = json!("fn demo");
        }),
        ("source_identity", |page| {
            page["items"][0]["unit"] = json!("rift://source/cargo/other@1.0.0/src/first.rs");
        }),
        ("declaration", |page| {
            page["items"][0]["declaration"]["range"] = json!({"start": 0, "end": 12});
        }),
        ("origin", |page| {
            page["items"][0]["declaration"]["symbol"]["origin"]["location"] = json!("project");
        }),
        ("duplicate_item", |page| {
            let hit = page["items"][0].clone();
            page["items"] = json!([hit.clone(), hit]);
        }),
    ];
    for (field, mutate) in mutations {
        let mut page = base();
        mutate(&mut page);
        assert_eq!(
            check(&request, &capabilities, page, None),
            Err(ClientError::InvalidResponseField { field })
        );
    }
}

#[test]
fn test_pattern_pages_refuse_past_their_bounds() {
    let capabilities = pattern_capabilities();
    let mut request = pattern_request();
    request.include = Some(vec!["source".to_owned()]);
    let mut sourced = pattern_page_json(None);
    sourced["items"][0]["source"] = json!("fn demo");
    sourced["items"][0]["declaration"]["source"] = json!("pub fn demo");
    assert_eq!(
        check(&request, &capabilities, sourced.clone(), None),
        Ok(())
    );
    let mut tight = capabilities.clone();
    tight.bounds.source_bytes_max = 3;
    assert_eq!(
        check(&request, &tight, sourced.clone(), None),
        Err(ClientError::InvalidResponseField { field: "source" })
    );
    // The declaration's source is bounded apart from the matched line's.
    tight.bounds.source_bytes_max = 8;
    assert_eq!(
        check(&request, &tight, sourced, None),
        Err(ClientError::InvalidResponseField { field: "source" })
    );

    let request = pattern_request();
    let mut over_limit = pattern_page_json(None);
    over_limit["items"] =
        Value::Array((0..21).map(|start| pattern_hit_json(UNIT, start)).collect());
    assert_eq!(
        check(&request, &capabilities, over_limit, None),
        Err(ClientError::InvalidResponseField { field: "items" })
    );

    let files = |count: usize| {
        let mut page = pattern_page_json(None);
        page["items"] = Value::Array(
            (0..count)
                .map(|file| {
                    pattern_hit_json(
                        &format!("rift://source/cargo/demo@1.0.0/src/first{file}.rs"),
                        10,
                    )
                })
                .collect(),
        );
        serde_json::from_value::<PackagePatternPage>(page).expect("pattern page")
    };
    let wide = PatternPageCheck {
        request: &request,
        capabilities: &capabilities,
        limit: PAGE_LIMIT_MAX,
        cursor: None,
    };
    for (count, expected) in [
        (PATTERN_PAGE_FILES_MAX, Ok(())),
        (
            PATTERN_PAGE_FILES_MAX + 1,
            Err(ClientError::InvalidResponseField { field: "files" }),
        ),
    ] {
        let mut page = files(count);
        for hit in &mut page.items {
            hit.declaration = None;
        }
        assert_eq!(wide.validate(&page), expected, "{count} files");
    }

    let mut stalled = pattern_page_json(None);
    stalled["items"] = json!([]);
    assert_eq!(
        check(&request, &capabilities, stalled, Some("next")),
        Err(ClientError::InvalidResponseField {
            field: "cursor_progress"
        })
    );
}

/// A match converts into the local read model: a file hit addressed by `unit` with the file's
/// size, and a symbol hit for the declaration holding it, both tagged `content`.
#[test]
fn test_a_pattern_match_converts_into_a_file_hit_and_its_declaration() {
    let mut value = pattern_hit_json(UNIT, 10);
    value["source"] = json!("fn demo");
    let hit: PackagePatternHit = serde_json::from_value(value).expect("pattern hit fixture");
    let matched = PackagePatternMatch::try_from(&hit).expect("a valid match converts");
    assert_eq!(matched.package.name, "demo");
    let file = &matched.file;
    assert_eq!(
        file.hit,
        rift_protocol::read::SearchHitTarget::File {
            size: FILE_SIZE,
            languages: Vec::new(),
        }
    );
    assert_eq!(file.unit.as_ref().map(|unit| unit.0.as_str()), Some(UNIT));
    assert_eq!(file.path, None);
    assert_eq!(file.source.as_deref(), Some("fn demo"));
    assert_eq!(
        file.range,
        Some(rift_protocol::read::TextRange { start: 10, end: 14 })
    );
    assert_eq!(file.line, Some(2));
    assert_eq!(
        file.matched_by,
        [rift_protocol::read::MatchedField::Content]
    );
    let declaration = matched
        .declaration
        .expect("the match names its declaration");
    let rift_protocol::read::SearchHitTarget::Symbol { symbol } = &declaration.hit else {
        panic!("a declaration answers a symbol hit: {declaration:?}");
    };
    assert_eq!(symbol.name, "demo");
    assert_eq!(
        declaration.range,
        Some(rift_protocol::read::TextRange { start: 0, end: 40 })
    );
    assert_eq!(
        declaration.unit.as_ref().map(|unit| unit.0.as_str()),
        Some(UNIT)
    );

    let mut outside = hit.clone();
    outside.size = 13;
    assert_eq!(
        PackagePatternMatch::try_from(&outside),
        Err(ClientError::InvalidResponseField { field: "location" })
    );
    let mut negative = hit.clone();
    negative.size = -1;
    assert_eq!(
        PackagePatternMatch::try_from(&negative),
        Err(ClientError::InvalidResponseField { field: "location" })
    );
    let mut foreign = hit;
    foreign.unit = "rift://source/cargo/other@1.0.0/src/first.rs".to_owned();
    assert_eq!(
        PackagePatternMatch::try_from(&foreign),
        Err(ClientError::InvalidResponseField {
            field: "source_identity"
        })
    );
}
