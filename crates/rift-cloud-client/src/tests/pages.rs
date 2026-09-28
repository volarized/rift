use super::*;

/// A paged read asks for the smaller of its limit and the advertised `page_limit_max`: pages of
/// 200 against a server advertising 200, one page per search phase against one advertising 1,000.
#[tokio::test]
async fn test_fixture_paged_reads_ask_for_the_advertised_page_limit() {
    for (advertised, asked) in [(200, "limit=200"), (1_000, "limit=1000")] {
        let (server, client) = operation_client(OperationFixture::PageLimitMax(advertised)).await;
        let pages = client
            .search_packages_pages(&search_request(), PAGE_LIMIT_MAX)
            .await;
        assert!(pages.is_ok(), "{pages:?}");
        assert_eq!(server.state.last_query.lock().await.as_deref(), Some(asked));
        let pages = client
            .list_package_symbols_pages(&symbol_request(), PAGE_LIMIT_MAX)
            .await;
        assert!(pages.is_ok(), "{pages:?}");
        let last_query = server.state.last_query.lock().await.clone();
        assert_eq!(
            last_query
                .as_deref()
                .and_then(|query| query.split('&').next()),
            Some(asked),
            "the symbol pages follow a cursor at the same limit"
        );
    }

    let (_server, client) = operation_client(OperationFixture::PageLimitMax(200)).await;
    assert_eq!(
        client
            .search_packages(&search_request(), PAGE_LIMIT_MAX, None)
            .await,
        Err(ClientError::InvalidRequest { field: "limit" }),
        "one page asked past the advertised bound stays refused"
    );
}

#[tokio::test]
async fn test_fixture_capabilities_accept_a_page_limit_up_to_the_client_bound() {
    let (_server, client) = operation_client(OperationFixture::PageLimitMax(PAGE_LIMIT_MAX)).await;
    let capabilities = client.get_capabilities().await;
    assert!(capabilities.is_ok(), "{capabilities:?}");

    for advertised in [PAGE_LIMIT_MAX + 1, 0] {
        let (_server, client) = operation_client(OperationFixture::PageLimitMax(advertised)).await;
        assert_eq!(
            client.get_capabilities().await,
            Err(ClientError::InvalidResponseField { field: "bounds" }),
            "{advertised}"
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
