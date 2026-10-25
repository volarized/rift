use super::*;
use crate::pattern::{
    PatternPageCheck, validate_pattern_request, validate_pattern_request_for_capabilities,
};
use rift_protocol::read::SEARCH_PATTERN_CHARS_MAX;
use serde_json::{Value, json};

const UNIT: &str = "rift://source/cargo/crates.io/demo@1.0.0/src/first.rs";

/// One edit to a pattern page fixture, beside the field the client names when it refuses it.
type PageEdit = (&'static str, fn(&mut Value));

/// Size of the file every fixture match sits in.
const FILE_SIZE: u64 = 64;

fn pattern_hit_json(unit: &str, start: u64) -> Value {
    let mut symbol = symbol_json("demo", "first");
    symbol["id"] = json!(pattern_symbol_identity(
        "demo",
        "crates.io",
        "1.0.0",
        "rust"
    ));
    json!({
        "package": package_json("demo"), "unit": unit,
        "range": {"start": start, "end": start + 4}, "line": 2, "size": FILE_SIZE,
        "declaration": {"symbol": symbol, "range": {"start": 0, "end": 40}, "line": 1}
    })
}

fn pattern_symbol_identity(package: &str, registry: &str, version: &str, language: &str) -> String {
    rift_protocol::identity::SymbolIdentity::new(
        rift_protocol::identity::SymbolOwner::Package {
            manager: "cargo".to_owned(),
            registry: registry.to_owned(),
            name: package.to_owned(),
            version: version.to_owned(),
        },
        rift_protocol::read::Language::from_identity_segment(language).expect("fixture language"),
        vec!["library".to_owned(), "demo".to_owned()],
    )
    .expect("canonical declaration identity")
    .wire_identity()
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

#[test]
fn pattern_pages_preserve_optional_original_views_including_complete_empty() {
    let mut value = pattern_page_json(None);
    value["items"] = serde_json::json!([]);
    value["next_cursor"] = serde_json::Value::Null;
    let older: crate::PackagePatternPage =
        serde_json::from_value(value.clone()).expect("older page");
    assert!(older.captured_view(None).expect("optional view").is_none());
    value["view"] = serde_json::json!({"id":"b".repeat(64),"expires_at":"2099-10-10T12:00:00Z"});
    let page: crate::PackagePatternPage =
        serde_json::from_value(value.clone()).expect("captured page");
    let original = page
        .captured_view(None)
        .expect("checked view")
        .expect("captured view");
    assert!(page.items.is_empty());
    assert_eq!(original.id.as_str(), "b".repeat(64));
    assert_eq!(
        page.captured_view(Some(&original)).expect("same view"),
        Some(original.clone())
    );
    assert_eq!(
        page.captured_view(None).expect("stable expiry"),
        Some(original.clone())
    );
    assert!(older.captured_view(Some(&original)).is_err());
    for change in ["identity", "expiry", "expired", "malformed"] {
        let mut changed = value.clone();
        match change {
            "identity" => changed["view"]["id"] = serde_json::json!("c".repeat(64)),
            "expiry" => changed["view"]["expires_at"] = serde_json::json!("2100-10-10T12:00:00Z"),
            "expired" => changed["view"]["expires_at"] = serde_json::json!("2020-10-10T12:00:00Z"),
            "malformed" => changed["view"]["id"] = serde_json::json!("opaque"),
            _ => unreachable!(),
        }
        let changed: crate::PackagePatternPage =
            serde_json::from_value(changed).expect("typed view fields");
        assert!(changed.captured_view(Some(&original)).is_err(), "{change}");
    }
    value["view"]["id"] = serde_json::json!("claims.signature");
    let opaque: crate::PackagePatternPage =
        serde_json::from_value(value.clone()).expect("opaque view");
    assert!(
        opaque
            .captured_view(None)
            .expect("existing view codec")
            .is_some()
    );
    value["view"]["expires_at"] = serde_json::json!("not a date");
    assert!(serde_json::from_value::<crate::PackagePatternPage>(value).is_err());
}

pub(super) fn pattern_request() -> PackagePatternRequest {
    PackagePatternRequest {
        pattern: r"fn\s+demo".to_owned(),
        packages: Some(vec![package_request()]),
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
) -> Result<(), RiftError> {
    let page: PackagePatternPage = serde_json::from_value(page).expect("pattern page fixture");
    PatternPageCheck {
        request,
        capabilities,
        limit: 20,
        cursor,
        source_bytes_max: SOURCE_BYTES_MAX,
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
    assert_registered_field(
        &client
            .search_package_patterns(&pattern_request(), 20, None)
            .await
            .expect_err("pattern feature refusal"),
        "rift.client.configuration_invalid",
        "feature",
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
    assert_registered_field(
        &client
            .search_package_patterns(&pattern_request(), 20, None)
            .await
            .expect_err("disabled pattern client"),
        "rift.client.configuration_invalid",
        "enabled",
    );
}

#[tokio::test]
async fn test_fixture_pattern_failure_marks_the_endpoint_unavailable() {
    let (_server, client) = operation_client(OperationFixture::Patterns).await;
    let mut request = pattern_request();
    request.packages.as_mut().expect("selected packages")[0].name = "other".to_owned();
    assert_registered_field(
        &client
            .search_package_patterns(&request, 20, None)
            .await
            .expect_err("registered refusal"),
        "rift.client.response_invalid",
        "package",
    );
    assert_registered_field(
        &client
            .search_package_patterns(&pattern_request(), 20, None)
            .await
            .expect_err("registered refusal"),
        "rift.client.response_invalid",
        "package",
    );
}

/// The contract's `maxLength` counts characters, so a pattern of that many two-byte
/// characters is accepted although it holds twice as many bytes, and one more refuses.
#[test]
fn test_the_pattern_bound_counts_characters_as_the_contract_does() {
    let mut request = pattern_request();
    request.pattern = "\u{e9}".repeat(SEARCH_PATTERN_CHARS_MAX);
    assert_eq!(request.pattern.len(), 2 * SEARCH_PATTERN_CHARS_MAX);
    validate_pattern_request(&request).expect("validation accepted");

    request.pattern.push('\u{e9}');
    assert_registered_field(
        &validate_pattern_request(&request).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "pattern",
    );
}

#[test]
fn test_pattern_requests_refuse_bounds_before_transport() {
    validate_pattern_request(&pattern_request()).expect("validation accepted");
    let cases = [
        (String::new(), "pattern"),
        ("x".repeat(SEARCH_PATTERN_CHARS_MAX + 1), "pattern"),
    ];
    for (pattern, field) in cases {
        let mut request = pattern_request();
        request.pattern = pattern;
        assert_registered_field(
            &validate_pattern_request(&request).expect_err("registered refusal"),
            "rift.client.request_invalid",
            field,
        );
    }
    let mut request = pattern_request();
    request.pattern = "x".repeat(SEARCH_PATTERN_CHARS_MAX);
    validate_pattern_request(&request).expect("validation accepted");

    for include in [
        vec!["score".to_owned()],
        vec!["source".to_owned(), "source".to_owned()],
    ] {
        let mut request = pattern_request();
        request.include = Some(include);
        assert_registered_field(
            &validate_pattern_request(&request).expect_err("registered refusal"),
            "rift.client.request_invalid",
            "include",
        );
    }
    let mut request = pattern_request();
    request.packages = Some(vec![package_request(), package_request()]);
    assert_registered_field(
        &validate_pattern_request(&request).expect_err("registered refusal"),
        "rift.client.request_invalid",
        "duplicate_package",
    );
    let capabilities = pattern_capabilities();
    assert_registered_field(
        &validate_pattern_request_for_capabilities(&pattern_request(), 0, None, &capabilities)
            .expect_err("registered refusal"),
        "rift.client.request_invalid",
        "limit",
    );
}

#[test]
fn test_discovery_patterns_accept_omitted_or_empty_package_selection() {
    for packages in [None, Some(Vec::new())] {
        let mut request = pattern_request();
        request.packages = packages;
        validate_pattern_request(&request).expect("validation accepted");
        let serialized = serde_json::to_value(&request).expect("pattern request");
        if request.packages.is_none() {
            assert!(serialized.get("packages").is_none());
        }
        check(
            &request,
            &pattern_capabilities(),
            pattern_page_json(None),
            None,
        )
        .expect("validation accepted");
    }
    let mut request = pattern_request();
    let mut other = package_request();
    other.name = "other".to_owned();
    request.packages = Some(vec![other]);
    assert_registered_field(
        &check(
            &request,
            &pattern_capabilities(),
            pattern_page_json(None),
            None,
        )
        .expect_err("registered refusal"),
        "rift.client.response_invalid",
        "package",
    );
}

#[test]
fn test_pattern_pages_refuse_matches_breaking_the_contract() {
    let request = pattern_request();
    let capabilities = pattern_capabilities();
    let base = || pattern_page_json(None);
    check(&request, &capabilities, base(), None).expect("validation accepted");

    let mut without_declaration = base();
    without_declaration["items"][0]
        .as_object_mut()
        .expect("hit object")
        .remove("declaration");
    check(&request, &capabilities, without_declaration, None).expect("validation accepted");

    let mutations: [PageEdit; 11] = [
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
            page["items"][0]["unit"] =
                json!("rift://source/cargo/crates.io/other@1.0.0/src/first.rs");
        }),
        ("source_identity", |page| {
            page["items"][0]["unit"] = json!("rift://source/npm/demo@1.0.0/src/first.rs");
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
        assert_registered_field(
            &check(&request, &capabilities, page, None).expect_err("registered refusal"),
            "rift.client.response_invalid",
            field,
        );
    }
}

#[test]
fn test_pattern_declarations_validate_canonical_owner_and_language() {
    let request = pattern_request();
    let capabilities = pattern_capabilities();
    let page = pattern_page_json(None);
    check(&request, &capabilities, page.clone(), None).expect("canonical declaration accepted");
    let hit: PackagePatternHit =
        serde_json::from_value(page["items"][0].clone()).expect("pattern hit fixture");
    let converted = PackagePatternMatch::try_from(&hit).expect("canonical declaration converts");
    let declaration = converted.declaration.expect("declaration retained");
    assert_eq!(
        declaration.unit.as_ref().map(|unit| unit.0.as_str()),
        Some(UNIT)
    );
    let rift_protocol::read::SearchHitTarget::Symbol { symbol } = declaration.hit else {
        panic!("declaration retains symbol");
    };
    assert_eq!(
        symbol.id.as_ref().map(|id| id.0.as_str()),
        hit.declaration
            .as_ref()
            .and_then(|declaration| declaration.symbol.id.as_deref())
    );

    let invalid_ids = [
        "not-an-identity".to_owned(),
        "rift://symbol/rust/cargo/demo@1.0.0/src/first.rs/demo".to_owned(),
        pattern_symbol_identity("other", "crates.io", "1.0.0", "rust"),
        pattern_symbol_identity("demo", "other.example", "1.0.0", "rust"),
        pattern_symbol_identity("demo", "crates.io", "2.0.0", "rust"),
        pattern_symbol_identity("demo", "crates.io", "1.0.0", "typescript"),
    ];
    for id in invalid_ids {
        let mut invalid_page = page.clone();
        invalid_page["items"][0]["declaration"]["symbol"]["id"] = json!(id);
        assert_registered_field(
            &check(&request, &capabilities, invalid_page.clone(), None)
                .expect_err("invalid canonical declaration refused"),
            "rift.client.response_invalid",
            "symbol_identity",
        );
        let invalid_hit: PackagePatternHit =
            serde_json::from_value(invalid_page["items"][0].clone()).expect("pattern hit fixture");
        assert_registered_field(
            &PackagePatternMatch::try_from(&invalid_hit).expect_err("invalid declaration refused"),
            "rift.client.response_invalid",
            "symbol_identity",
        );
    }
    let mut missing = page;
    missing["items"][0]["declaration"]["symbol"]
        .as_object_mut()
        .expect("symbol object")
        .remove("id");
    assert_registered_field(
        &check(&request, &capabilities, missing, None).expect_err("missing identity refused"),
        "rift.client.response_invalid",
        "symbol_identity",
    );
}

#[test]
fn test_pattern_pages_refuse_past_their_bounds() {
    let capabilities = pattern_capabilities();
    let mut request = pattern_request();
    request.include = Some(vec!["source".to_owned()]);
    let mut sourced = pattern_page_json(None);
    sourced["items"][0]["source"] = json!("fn demo");
    sourced["items"][0]["declaration"]["source"] = json!("pub fn demo");
    check(&request, &capabilities, sourced.clone(), None).expect("validation accepted");
    let mut tight = capabilities.clone();
    tight.bounds.source_bytes_max = 3;
    assert_registered_field(
        &check(&request, &tight, sourced.clone(), None).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "source",
    );
    // The declaration's source is bounded apart from the matched line's.
    tight.bounds.source_bytes_max = 8;
    assert_registered_field(
        &check(&request, &tight, sourced, None).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "source",
    );

    let request = pattern_request();
    let mut over_limit = pattern_page_json(None);
    over_limit["items"] =
        Value::Array((0..21).map(|start| pattern_hit_json(UNIT, start)).collect());
    assert_registered_field(
        &check(&request, &capabilities, over_limit, None).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "items",
    );

    let files = |count: usize| {
        let mut page = pattern_page_json(None);
        page["items"] = Value::Array(
            (0..count)
                .map(|file| {
                    pattern_hit_json(
                        &format!("rift://source/cargo/crates.io/demo@1.0.0/src/first{file}.rs"),
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
        source_bytes_max: SOURCE_BYTES_MAX,
    };
    for (count, accepted) in [
        (PATTERN_PAGE_FILES_MAX, true),
        (PATTERN_PAGE_FILES_MAX + 1, false),
    ] {
        let mut page = files(count);
        for hit in &mut page.items {
            hit.declaration = None;
        }
        let result = wide.validate(&page);
        if accepted {
            result.expect("exact file bound accepted");
        } else {
            assert_registered_field(
                &result.expect_err("file bound refusal"),
                "rift.client.response_invalid",
                "files",
            );
        }
    }

    let mut stalled = pattern_page_json(None);
    stalled["items"] = json!([]);
    assert_registered_field(
        &check(&request, &capabilities, stalled, Some("next")).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "cursor_progress",
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
    assert_registered_field(
        &PackagePatternMatch::try_from(&outside).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "location",
    );
    let mut negative = hit.clone();
    negative.size = -1;
    assert_registered_field(
        &PackagePatternMatch::try_from(&negative).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "location",
    );
    let mut foreign = hit;
    foreign.unit = "rift://source/cargo/crates.io/other@1.0.0/src/first.rs".to_owned();
    assert_registered_field(
        &PackagePatternMatch::try_from(&foreign).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "source_identity",
    );

    let mut value = pattern_hit_json(UNIT, 10);
    value["declaration"]["symbol"]["origin"]["location"] = json!("project");
    let project: PackagePatternHit = serde_json::from_value(value).expect("pattern hit fixture");
    assert_registered_field(
        &PackagePatternMatch::try_from(&project).expect_err("registered refusal"),
        "rift.client.response_invalid",
        "origin",
    );
}
