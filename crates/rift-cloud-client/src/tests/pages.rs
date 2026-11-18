use super::*;

/// A paged read asks for the smaller of its limit and the advertised `page_limit_max`: pages of
/// 200 against a server advertising 200, one page per search phase against one advertising 1,000.
#[tokio::test]
async fn test_fixture_paged_reads_ask_for_the_advertised_page_limit() {
    for (advertised, asked) in [
        (200, "page=1&max_results=200"),
        (1_000, "page=1&max_results=1000"),
    ] {
        let (server, client) = operation_client(OperationFixture::PageLimitMax(advertised)).await;
        let pages = client
            .search_packages_pages(&search_request(), PAGE_LIMIT_MAX)
            .await;
        assert!(pages.is_ok(), "{pages:?}");
        assert_eq!(server.state.last_query.lock().await.as_deref(), Some(asked));
        let result = client
            .get_symbol(&symbol_request())
            .await
            .expect("exact read");
        assert!(
            matches!(result, GetSymbolResult::Found { ref declarations, .. } if declarations.len() == 1)
        );
        assert_eq!(server.state.last_query.lock().await.as_deref(), None);
        let requests = server.state.request_log.lock().await;
        let sent = requests.last().expect("exact request");
        let body: serde_json::Value = serde_json::from_slice(&sent.body).expect("request body");
        assert_eq!(body["max_results"], 10);
    }

    let (_server, client) = operation_client(OperationFixture::PageLimitMax(200)).await;
    assert_registered_field(
        &client
            .search_packages(&search_request(), PAGE_LIMIT_MAX, 1)
            .await
            .expect_err("one page asked past the advertised bound stays refused"),
        "rift.client.request_invalid",
        "max_results",
    );
}

#[tokio::test]
async fn test_fixture_capabilities_accept_a_page_limit_up_to_the_client_bound() {
    let (_server, client) = operation_client(OperationFixture::PageLimitMax(PAGE_LIMIT_MAX)).await;
    let capabilities = client.get_capabilities().await;
    assert!(capabilities.is_ok(), "{capabilities:?}");

    for advertised in [PAGE_LIMIT_MAX + 1, 0] {
        let (_server, client) = operation_client(OperationFixture::PageLimitMax(advertised)).await;
        assert_registered_field(
            &client
                .get_capabilities()
                .await
                .expect_err("advertised page bound refusal"),
            "rift.client.response_invalid",
            "bounds",
        );
    }
}

#[test]
fn test_advertised_page_limit_cuts_the_caller_limit() {
    let mut capabilities: Capabilities =
        serde_json::from_str(&capabilities_json()).expect("capabilities fixture");
    capabilities.bounds.page_limit_max = 200;
    assert_eq!(advertised_page_limit(PAGE_LIMIT_MAX, &capabilities), 200);
    assert_eq!(advertised_page_limit(20, &capabilities), 20);
}

/// The first page stopped at the response body bound, marked `result_truncated`, and the page
/// that follows it.
pub(super) fn body_bound_page(
    mut first: serde_json::Value,
    second: serde_json::Value,
    selected_page: Option<&str>,
) -> serde_json::Value {
    if selected_page.is_some() {
        return second;
    }
    first["warnings"] = serde_json::json!([
        {"code": "result_truncated", "detail": "the page stopped at the response body bound"}
    ]);
    first
}

#[tokio::test]
async fn a_short_page_above_its_reported_total_refuses() {
    let (_server, client) = operation_client(OperationFixture::BodyBoundStop).await;
    let error = client
        .search_packages(&search_request(), 20, 1)
        .await
        .expect_err("short page");
    assert_registered_field(&error, "rift.client.response_invalid", "pagination");
}

/// Symbol page assembly refuses a page that changes revision, repeats a declaration, or
/// repeats a page, and records the refusal a later request answers; it stops at the
/// advertised candidate bound.
#[tokio::test]
async fn test_fixture_search_page_assembly_records_refusals() {
    let refusals = [
        (OperationFixture::PageRevisionChange, "revision"),
        (OperationFixture::RepeatedPage, "duplicate_item"),
        (OperationFixture::RepeatedItem, "duplicate_item"),
    ];
    for (mode, field) in refusals {
        let (server, client) = operation_client(mode).await;
        let error = client
            .search_packages_pages(&search_request(), 1)
            .await
            .expect_err("page assembly refusal");
        assert_registered_field(&error, "rift.client.response_invalid", field);
        assert_eq!(server.state.requests.load(Ordering::SeqCst), 3, "{field}");
        let cached = client
            .get_capabilities()
            .await
            .expect_err("recorded page refusal");
        assert_eq!(cached.to_string(), error.to_string(), "{field} is recorded");
        assert_eq!(server.state.requests.load(Ordering::SeqCst), 3, "{field}");
    }

    let (server, client) = operation_client(OperationFixture::CandidateBound).await;
    let pages = client
        .search_packages_pages(&search_request(), 1)
        .await
        .expect("candidate bound page");
    assert_eq!(pages.items.len(), 1);
    assert_eq!(server.state.requests.load(Ordering::SeqCst), 2);
}

/// Exact declaration reads send the requested page and count unchanged.
#[tokio::test]
async fn exact_declaration_page_keeps_its_bounds() {
    let mut request = symbol_request();
    request.page = 1;
    request.max_results = 1;
    let (server, client) = operation_client(OperationFixture::Valid).await;
    client.get_symbol(&request).await.expect("exact page");
    let requests = server.state.request_log.lock().await;
    let body: serde_json::Value =
        serde_json::from_slice(&requests.last().expect("request").body).expect("body");
    assert_eq!(body["page"], 1);
    assert_eq!(body["max_results"], 1);
}
