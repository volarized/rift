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
    let owner =
        json!({"source":{"kind":"package","unit":"rift://source/cargo/demo@1.0.0/README.md"}});
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
            "target":symbol_json("demo","first")["id"],"evidence":"unique_name",
            "authored":"demo","range":{"start":1,"end":5}},"documentation":hit(),"excerpt":"guide"}]
    })
}

fn page() -> Value {
    let mut value = search_page_json("demo", None, "analyzer-v1", "first");
    value["documentation_revision"] = json!("0123abcd");
    value["items"] = json!([{"target":"documentation","package":package_json("demo"),
        "documentation":hit(),"source":"guide","contributing_fields":["content","documentation","future_field"]}]);
    value
}

fn request() -> PackageSearchRequest {
    let mut value = search_request();
    value.target = Some(PackageSearchRequestTarget::Documentation);
    value.include = Some(vec!["source".to_owned()]);
    value
}

fn refuses_search(value: Value, field: &'static str) {
    let value = serde_json::from_value(value).expect("generated page");
    assert!(
        matches!(validate_search_page(&request(), &capabilities(), &value, None),
        Err(ClientError::InvalidResponseField {field: actual}) if actual == field),
        "expected {field}"
    );
}

#[test]
fn documentation_pages_validate_identity_fields_and_requested_source() {
    let value: PackageSearchPage = serde_json::from_value(page()).expect("generated page");
    validate_search_page(&request(), &capabilities(), &value, None)
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
    assert!(matches!(
        validate_search_page(&without_source, &capabilities(), &value, None),
        Err(ClientError::InvalidResponseField { field: "source" })
    ));
    assert!(validate_search_page(&search_request(), &capabilities(), &value, None).is_err());
    let mut no_feature = capabilities();
    no_feature
        .supported_features
        .retain(|feature| feature != "documentation_search");
    assert!(validate_search_page(&request(), &no_feature, &value, None).is_err());
}

#[test]
fn documentation_pages_refuse_mixed_packages_revisions_and_invalid_metadata() {
    for (pointer, replacement, field) in [
        ("/items/0/package/name", json!("other"), "documentation"),
        (
            "/items/0/documentation/documentation_revision",
            json!("abcdef01"),
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

#[test]
fn symbol_context_matches_requested_symbol_and_revision() {
    let mut value = symbol_page_json("demo", None, "analyzer-v1", "first");
    value["documentation_revision"] = json!("0123abcd");
    value["items"][0]["documentation"] = context();
    let page: PackageSymbolPage = serde_json::from_value(value.clone()).expect("symbol page");
    let mut request = symbol_request();
    request.include = Some(vec![PackageSymbolRequestInclude::Documentation]);
    validate_symbol_page(&request, &capabilities(), &page, None).expect("valid symbol context");
    PackageSymbolCandidate::try_from(&page.items[0]).expect("context converts with symbol");
    assert!(validate_symbol_page(&symbol_request(), &capabilities(), &page, None).is_err());
    for (pointer, replacement) in [
        (
            "/items/0/documentation/documentation_revision",
            json!("abcdef01"),
        ),
        (
            "/items/0/documentation/references/0/reference/target",
            json!("rift://symbol/rust/src/other.rs/demo"),
        ),
        (
            "/items/0/documentation/references/0/documentation/source/origin/package/name",
            json!("other"),
        ),
    ] {
        let mut invalid = value.clone();
        *invalid.pointer_mut(pointer).expect("fixture field") = replacement;
        let page = serde_json::from_value(invalid).expect("generated symbol page");
        assert!(validate_symbol_page(&request, &capabilities(), &page, None).is_err());
    }
}

#[test]
fn documentation_capabilities_require_matching_revision() {
    for revision in [None, Some("invalid".to_owned())] {
        let mut value = capabilities();
        value.documentation_revision = revision;
        assert!(matches!(
            validate_capabilities(&value),
            Err(ClientError::InvalidResponseField {
                field: "documentation_revision"
            })
        ));
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
            let generated = serde_json::from_value(value.clone()).expect("generated context");
            let expected: rift_protocol::documentation::DocumentationContext =
                serde_json::from_value(value).expect("protocol context");
            assert_eq!(
                domain::protocol_documentation_context(generated).expect("converted context"),
                expected
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
        let generated = serde_json::from_value(value.clone()).expect("generated context");
        let expected: rift_protocol::documentation::DocumentationContext =
            serde_json::from_value(value).expect("protocol context");
        assert_eq!(
            domain::protocol_documentation_context(generated).expect("converted context"),
            expected
        );
    }
}
