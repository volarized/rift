use super::*;
use serde_json::{Value, json};

fn capabilities() -> Capabilities {
    let mut value: Capabilities = serde_json::from_str(&capabilities_json()).expect("capabilities");
    value.supported_features.extend([
        "documentation_search".to_owned(),
        "symbol_documentation".to_owned(),
    ]);
    value.documentation_revision = Some("0123abcd".to_owned());
    value
}

fn hit() -> Value {
    let owner = json!({"source":{"kind":"package","unit":"rift://source/cargo/crates.io/demo@1.0.0/README.md"}});
    json!({
        "documentation_revision":"0123abcd",
        "block":{"identity":"1".repeat(64),"source":owner,"content_digest":"2".repeat(64),
            "range":{"start":0,"end":40},"line":1,"kind":"prose"},
        "source":{"identity":owner,"revision":"3".repeat(64),"content_digest":"4".repeat(64),
            "origin":{"location":"dependency","package":package_json("demo"),"source_kind":"authored"},
            "format":"markdown","media_type":"text/markdown","selection":"package_archive","byte_length":40}
    })
}

fn context() -> Value {
    json!({
        "documentation_revision":"0123abcd","truncated":false,
        "references":[{"reference":{"identity":"5".repeat(64),"block":"1".repeat(64),
            "target":symbol_request().id,"evidence":"unique_name",
            "authored":"demo","range":{"start":1,"end":5}},"documentation":hit(),"excerpt":"guide"}]
    })
}

pub(super) fn page() -> Value {
    let mut value = search_page_json("demo", None, "analyzer-v1", "first");
    value["documentation_revision"] = json!("0123abcd");
    value["items"] = json!([{"target":"documentation","package":package_json("demo"),
        "documentation":hit(),"source":"guide","contributing_fields":["content","documentation","future_field"]}]);
    value
}

fn stored_pages() -> (Value, Value) {
    let mut search = page();
    search["items"][0]["documentation"]["documentation_revision"] = json!("abcdef01");
    let mut current = search["items"][0].clone();
    current["package"] = package_json("other");
    current["documentation"]["documentation_revision"] = json!("0123abcd");
    current["documentation"]["block"]["identity"] = json!("6".repeat(64));
    current["documentation"]["source"]["origin"]["package"] = package_json("other");
    current["documentation"]["source"]["identity"]["source"]["unit"] =
        json!("rift://source/cargo/crates.io/other@1.0.0/README.md");
    current["documentation"]["block"]["source"] =
        current["documentation"]["source"]["identity"].clone();
    search["items"]
        .as_array_mut()
        .expect("documentation items")
        .push(current);

    let mut symbols = symbol_result_json("demo", "first");
    symbols["declarations"][0]["source"] = json!("demo");
    let mut old_context = context();
    old_context["documentation_revision"] = json!("abcdef01");
    old_context["references"][0]["documentation"]["documentation_revision"] = json!("abcdef01");
    symbols["documentation"] = old_context;
    (search, symbols)
}

fn mixed_search_request() -> PackageSearchRequest {
    let mut request = request();
    let mut other = package_request();
    other.name = "other".to_owned();
    request.packages = Some(vec![package_request(), other]);
    request
}

fn mixed_symbol_request() -> GetSymbolParams {
    let mut request = symbol_request();
    request.include = vec![GetSymbolInclude::Source, GetSymbolInclude::Documentation];
    request
}

#[tokio::test]
async fn native_http_reads_preserve_old_and_current_documentation() {
    let (search, symbols) = stored_pages();
    let expected_search: PackageSearchPage =
        serde_json::from_value(search.clone()).expect("stored search");
    let (_server, client) = operation_client(OperationFixture::Documentation {
        search,
        symbols: symbols.clone(),
    })
    .await;
    assert_eq!(
        client
            .search_packages(&mixed_search_request(), 20, None)
            .await
            .expect("stored search"),
        expected_search
    );
    for revision in ["abcdef01", "0123abcd"] {
        let mut value = symbols.clone();
        value["documentation"]["documentation_revision"] = json!(revision);
        value["documentation"]["references"][0]["documentation"]["documentation_revision"] =
            json!(revision);
        let expected: GetSymbolResult =
            serde_json::from_value(value.clone()).expect("stored symbol");
        let actual = canonical_symbol_response(value, &mixed_symbol_request(), SOURCE_BYTES_MAX)
            .await
            .expect("stored symbol");
        assert_eq!(actual, expected);
        let GetSymbolResult::Found {
            declarations,
            documentation,
            ..
        } = actual
        else {
            panic!("found symbol")
        };
        assert_eq!(declarations[0].source.as_deref(), Some("demo"));
        let context = documentation.expect("requested context");
        assert_eq!(context.references.len(), 1);
        assert_eq!(context.documentation_revision.0, revision);
    }
}

#[tokio::test]
async fn native_http_all_search_keeps_lexical_source_beside_stored_documentation() {
    let (mut search, symbols) = stored_pages();
    let mut lexical = search_page_json("demo", None, "analyzer-v1", "first")["items"][0].clone();
    lexical["source"] = json!("demo");
    search["items"]
        .as_array_mut()
        .expect("search items")
        .push(lexical);
    let expected: PackageSearchPage =
        serde_json::from_value(search.clone()).expect("mixed target page");
    let (_server, client) =
        operation_client(OperationFixture::Documentation { search, symbols }).await;
    let mut request = mixed_search_request();
    request.target = Some(PackageSearchRequestTarget::All);
    let actual = client
        .search_packages(&request, 20, None)
        .await
        .expect("lexical and documentation reads");
    assert_eq!(actual, expected);
    for item in actual.items {
        let candidate = PackageSearchCandidate::try_from(item).expect("candidate converts");
        assert!(candidate.hit.source.is_some());
    }
}

#[tokio::test]
async fn native_http_stored_documentation_refuses_foreign_origins_and_invalid_revisions() {
    for (pointer, replacement, field) in [
        (
            "/items/0/documentation/source/origin/package/name",
            json!("foreign"),
            "documentation",
        ),
        (
            "/items/0/documentation/documentation_revision",
            json!("invalid"),
            "documentation",
        ),
        (
            "/documentation_revision",
            json!("abcdef01"),
            "documentation_revision",
        ),
        (
            "/items/0/documentation/block/range/end",
            json!(41),
            "documentation",
        ),
    ] {
        let (mut search, symbols) = stored_pages();
        *search.pointer_mut(pointer).expect("stored field") = replacement;
        let (_server, client) =
            operation_client(OperationFixture::Documentation { search, symbols }).await;
        assert_registered_field(
            &client
                .search_packages(&mixed_search_request(), 20, None)
                .await
                .expect_err("stored documentation refusal"),
            "rift.client.response_invalid",
            field,
        );
    }
}

#[tokio::test]
async fn native_http_stored_context_refuses_wrong_targets_revisions_and_duplicate_references() {
    let (_, original) = stored_pages();
    for (pointer, replacement) in [
        (
            "/documentation/references/0/documentation/documentation_revision",
            json!("0123abcd"),
        ),
        (
            "/documentation/references/0/reference/target",
            json!("rift://symbol/cargo/crates.io/other@1.0.0/rust/other"),
        ),
        (
            "/documentation/references/0/documentation/source/origin/package/name",
            json!("foreign"),
        ),
        ("/documentation/references/0/reference/range/end", json!(41)),
        ("/documentation/documentation_revision", json!("invalid")),
    ] {
        let mut value = original.clone();
        *value.pointer_mut(pointer).expect("stored field") = replacement;
        let error = canonical_symbol_response(value, &mixed_symbol_request(), SOURCE_BYTES_MAX)
            .await
            .expect_err("stored context refusal");
        assert_registered_field(&error, "rift.client.response_invalid", "body");
    }
    let mut value = original;
    let reference = value["documentation"]["references"][0].clone();
    value["documentation"]["references"]
        .as_array_mut()
        .expect("references")
        .push(reference);
    let error = canonical_symbol_response(value, &mixed_symbol_request(), SOURCE_BYTES_MAX)
        .await
        .expect_err("duplicate reference");
    assert_registered_field(&error, "rift.client.response_invalid", "body");
}

fn request() -> PackageSearchRequest {
    let mut value = search_request();
    value.target = Some(PackageSearchRequestTarget::Documentation);
    value.include = Some(vec!["source".to_owned()]);
    value
}

fn refuses_search(value: Value, field: &'static str) {
    let value = serde_json::from_value(value).expect("generated page");
    assert_registered_field(
        &validate_search_page(&request(), &capabilities(), &value, None, SOURCE_BYTES_MAX)
            .expect_err("documentation refusal"),
        "rift.client.response_invalid",
        field,
    );
}

#[test]
fn documentation_pages_validate_identity_fields_and_requested_source() {
    let value: PackageSearchPage = serde_json::from_value(page()).expect("generated page");
    validate_search_page(&request(), &capabilities(), &value, None, SOURCE_BYTES_MAX)
        .expect("valid documentation page");
    let candidate = PackageSearchCandidate::try_from(value.items[0].clone()).expect("candidate");
    assert_eq!(
        candidate.hit.matched_by,
        [
            rift_protocol::read::MatchedField::Content,
            rift_protocol::read::MatchedField::Documentation,
            rift_protocol::read::MatchedField::Ranked
        ]
    );
    let mut without_source = request();
    without_source.include = None;
    assert_registered_field(
        &validate_search_page(
            &without_source,
            &capabilities(),
            &value,
            None,
            SOURCE_BYTES_MAX,
        )
        .expect_err("unrequested documentation source refusal"),
        "rift.client.response_invalid",
        "source",
    );
    assert!(
        validate_search_page(
            &search_request(),
            &capabilities(),
            &value,
            None,
            SOURCE_BYTES_MAX
        )
        .is_err()
    );
    let mut no_feature = capabilities();
    no_feature
        .supported_features
        .retain(|feature| feature != "documentation_search");
    assert!(validate_search_page(&request(), &no_feature, &value, None, SOURCE_BYTES_MAX).is_err());
}

#[test]
fn discovery_documentation_accepts_omitted_or_empty_package_selection() {
    let value: PackageSearchPage = serde_json::from_value(page()).expect("documentation page");
    for packages in [None, Some(Vec::new())] {
        let mut request = request();
        request.packages = packages;
        validate_search_page(&request, &capabilities(), &value, None, SOURCE_BYTES_MAX)
            .expect("discovery documentation");
    }
    let mut request = request();
    let mut other = package_request();
    other.name = "other".to_owned();
    request.packages = Some(vec![other]);
    assert_registered_field(
        &validate_search_page(&request, &capabilities(), &value, None, SOURCE_BYTES_MAX)
            .expect_err("registered refusal"),
        "rift.client.response_invalid",
        "documentation",
    );
}

#[test]
fn documentation_pages_refuse_foreign_packages_and_invalid_metadata() {
    for (pointer, replacement, field) in [
        ("/items/0/package/name", json!("other"), "documentation"),
        (
            "/items/0/documentation/documentation_revision",
            json!("invalid"),
            "documentation",
        ),
        (
            "/items/0/contributing_fields",
            json!([]),
            "contributing_fields",
        ),
        (
            "/documentation_revision",
            json!("abcdef01"),
            "documentation_revision",
        ),
        ("/analyzer_revision", json!(""), "revision"),
        (
            "/publication_format",
            json!("rift-package-index-v1"),
            "publication_format",
        ),
        ("/next_cursor", json!(""), "cursor"),
        (
            "/warnings",
            json!([{"code":"source_unavailable","detail":""}]),
            "warnings",
        ),
    ] {
        let mut value = page();
        *value.pointer_mut(pointer).expect("fixture field") = replacement;
        refuses_search(value, field);
    }
    let mut duplicate = page();
    let item = duplicate["items"][0].clone();
    duplicate["items"].as_array_mut().expect("items").push(item);
    refuses_search(duplicate, "duplicate_item");
    let mut symbol = search_page_json("demo", None, "analyzer-v1", "first");
    symbol["documentation_revision"] = json!("0123abcd");
    refuses_search(symbol, "target");
}

#[test]
fn documentation_excerpt_bound_covers_total_page() {
    let maximum = rift_protocol::documentation::DOCUMENTATION_EXCERPT_BYTES_MAX as usize;
    let bytes = maximum / 2 + 1;
    let mut value = page();
    value["items"][0]["documentation"]["block"]["range"]["end"] = json!(bytes);
    value["items"][0]["documentation"]["source"]["byte_length"] = json!(bytes);
    value["items"][0]["source"] = json!("x".repeat(bytes));
    let mut second = value["items"][0].clone();
    second["documentation"]["block"]["identity"] = json!("6".repeat(64));
    value["items"].as_array_mut().expect("items").push(second);
    refuses_search(value, "source");
}

#[tokio::test]
async fn symbol_context_matches_requested_symbol_and_revision() {
    let mut value = symbol_result_json("demo", "first");
    value["documentation"] = context();
    let request = mixed_symbol_request();
    canonical_symbol_response(value.clone(), &request, SOURCE_BYTES_MAX)
        .await
        .expect("valid context");
    let mut absent = request.clone();
    absent.include = vec![GetSymbolInclude::Source];
    assert!(
        canonical_symbol_response(value.clone(), &absent, SOURCE_BYTES_MAX)
            .await
            .is_err()
    );
    for (pointer, replacement) in [
        ("/documentation/documentation_revision", json!("abcdef01")),
        (
            "/documentation/references/0/reference/target",
            json!("rift://symbol/cargo/crates.io/other@1.0.0/rust/other"),
        ),
        (
            "/documentation/references/0/documentation/source/origin/package/name",
            json!("other"),
        ),
    ] {
        let mut invalid = value.clone();
        *invalid.pointer_mut(pointer).expect("context field") = replacement;
        assert!(
            canonical_symbol_response(invalid, &request, SOURCE_BYTES_MAX)
                .await
                .is_err()
        );
    }
    value["documentation"]["documentation_revision"] = json!("abcdef01");
    value["documentation"]["references"][0]["documentation"]["documentation_revision"] =
        json!("abcdef01");
    canonical_symbol_response(value, &request, SOURCE_BYTES_MAX)
        .await
        .expect("matching stored revision");
}

#[tokio::test]
async fn symbol_source_requires_source_include() {
    let mut value = symbol_result_json("demo", "first");
    value["declarations"][0]["source"] = json!("demo");
    let mut request = symbol_request();
    request.include = vec![GetSymbolInclude::Source];
    canonical_symbol_response(value.clone(), &request, SOURCE_BYTES_MAX)
        .await
        .expect("requested source");
    request.include.clear();
    let error = canonical_symbol_response(value, &request, SOURCE_BYTES_MAX)
        .await
        .expect_err("unrequested source");
    assert_registered_field(&error, "rift.client.response_invalid", "body");
}

#[test]
fn documentation_capabilities_require_matching_revision() {
    for revision in [None, Some("invalid".to_owned())] {
        let mut value = capabilities();
        value.documentation_revision = revision;
        assert_registered_field(
            &validate_capabilities(&value).expect_err("documentation revision refusal"),
            "rift.client.response_invalid",
            "documentation_revision",
        );
    }
}

#[test]
fn generated_context_preserves_optional_metadata_and_enum_values() {
    for (format, selection) in [
        ("mdx", "package_archive"),
        ("restructured_text", "cloud_resolver"),
        ("text", "package_archive"),
        ("notebook", "package_archive"),
        ("attached_comment", "attached_comment"),
    ] {
        for evidence in ["provider", "authored_link", "qualified_name", "unique_name"] {
            let mut value = context();
            value["references"][0]["reference"]["evidence"] = json!(evidence);
            let hit = &mut value["references"][0]["documentation"];
            hit["source"]["format"] = json!(format);
            hit["source"]["selection"] = json!(selection);
            hit["source"]["license"] =
                json!({"expression":"MIT","files":[{"path":"LICENSE","digest":"a".repeat(64)}]});
            hit["block"]["kind"] = json!("code");
            hit["block"]["heading_path"] = json!([{"level":1,"name":"Guide"}]);
            hit["block"]["chunks"] =
                json!([{"identity":"text:fixture","range":{"start":0,"end":40}}]);
            let generated: crate::DocumentationContext =
                serde_json::from_value(value.clone()).expect("generated context");
            let expected: rift_protocol::documentation::DocumentationContext =
                serde_json::from_value(value).expect("protocol context");
            assert_eq!(
                generated,
                serde_json::from_value::<crate::DocumentationContext>(
                    serde_json::to_value(&expected).expect("shared wire")
                )
                .expect("generated context from shared wire")
            );
        }
    }
}

#[test]
fn generated_context_preserves_notebook_cells_and_warnings() {
    for (identity, kind) in [
        (json!({"kind":"authored","id":"cell-one"}), "markdown"),
        (json!({"kind":"indexed","index":3}), "code"),
    ] {
        let mut value = context();
        let cell = json!({"identity":identity,"kind":kind});
        value["references"][0]["documentation"]["source"]["identity"]["cell"] = cell.clone();
        value["references"][0]["documentation"]["block"]["source"]["cell"] = cell;
        let source = value["references"][0]["documentation"]["source"]["identity"].clone();
        value["warnings"] = json!(["source_unavailable","source_truncated","unsupported_format","malformed_source","omitted_range","limit_exceeded"]
            .into_iter().enumerate().map(|(index,kind)| json!({"source":source,"stage":(["source","extract","resolve","index"][index % 4]),"kind":kind,"count":1})).collect::<Vec<_>>());
        let generated: crate::DocumentationContext =
            serde_json::from_value(value.clone()).expect("generated context");
        let expected: rift_protocol::documentation::DocumentationContext =
            serde_json::from_value(value).expect("protocol context");
        assert_eq!(
            generated,
            serde_json::from_value::<crate::DocumentationContext>(
                serde_json::to_value(&expected).expect("shared wire")
            )
            .expect("generated context from shared wire")
        );
    }
}
