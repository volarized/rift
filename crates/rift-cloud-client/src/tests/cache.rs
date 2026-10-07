use super::{
    FixtureMode, FixtureServer, GlobalClient, OperationFixture, StatusCode, operation_client,
    resolution_request,
};
use rift_tracing::{ScopedRecorder, SeriesValue};

fn assert_entries(recorder: &ScopedRecorder, cache_name: &str, expected: f64) {
    let metrics = recorder.metrics();
    let labels = [("cache.name", cache_name)];
    let series = metrics
        .find("cache.entry.count", &labels)
        .expect("the client observes its retained entries");
    assert_eq!(series.scope_name(), env!("CARGO_PKG_NAME"));
    assert_eq!(series.unit(), "{entry}");
    assert_eq!(series.value(), &SeriesValue::Sum(expected));
}

#[tokio::test]
async fn test_cache_entries_sum_owners_and_follow_final_clone_drop() {
    let (recorder, _drain) = ScopedRecorder::builder().install().expect("recorder");
    let (server, client) = operation_client(OperationFixture::EchoResolution).await;
    let second = GlobalClient::new(server.config()).expect("second client");
    let clone = client.clone();
    assert_entries(&recorder, "CachedCapabilities", 0.0);
    assert_entries(&recorder, "CachedResolution", 0.0);
    assert_entries(&recorder, "CachedFailure", 0.0);

    let mut request = resolution_request();
    assert!(client.resolve_package_context(&request).await.is_ok());
    assert!(second.resolve_package_context(&request).await.is_ok());
    assert_entries(&recorder, "CachedCapabilities", 2.0);
    assert_entries(&recorder, "CachedResolution", 2.0);
    request.entries[0].name = "demo-next".to_owned();
    assert!(clone.resolve_package_context(&request).await.is_ok());
    assert_entries(&recorder, "CachedResolution", 2.0);

    drop(client);
    assert_entries(&recorder, "CachedResolution", 2.0);
    drop(second);
    assert_entries(&recorder, "CachedCapabilities", 1.0);
    assert_entries(&recorder, "CachedResolution", 1.0);
    drop(clone);
    for cache_name in ["CachedCapabilities", "CachedResolution", "CachedFailure"] {
        let labels = [("cache.name", cache_name)];
        assert!(
            recorder
                .metrics()
                .find("cache.entry.count", &labels)
                .is_none()
        );
    }
}

#[tokio::test]
async fn test_cache_entries_count_expired_failure_until_capabilities_clear_it() {
    let (recorder, _drain) = ScopedRecorder::builder().install().expect("recorder");
    let server = FixtureServer::start(FixtureMode::StatusSequence(vec![
        StatusCode::UNAUTHORIZED,
        StatusCode::UNAUTHORIZED,
        StatusCode::OK,
        StatusCode::OK,
    ]))
    .await
    .expect("fixture server");
    let client = GlobalClient::new(server.config()).expect("fixture client");
    assert!(client.get_capabilities().await.is_err());
    assert_entries(&recorder, "CachedFailure", 1.0);
    assert_entries(&recorder, "CachedCapabilities", 0.0);
    expire_failure(&client).await;
    assert_entries(&recorder, "CachedFailure", 1.0);
    assert!(client.get_capabilities().await.is_err());
    assert_entries(&recorder, "CachedFailure", 1.0);
    expire_failure(&client).await;
    assert!(client.get_capabilities().await.is_ok());
    assert_entries(&recorder, "CachedCapabilities", 1.0);
    assert_entries(&recorder, "CachedFailure", 0.0);
    assert!(client.get_capabilities().await.is_ok());
    assert_entries(&recorder, "CachedCapabilities", 1.0);
    assert_entries(&recorder, "CachedFailure", 0.0);
}

async fn expire_failure(client: &GlobalClient) {
    let mut failure = client.inner.failure.write().await;
    failure.as_mut().expect("retained failure").expires = tokio::time::Instant::now();
}
