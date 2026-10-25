use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use rift_protocol::query::{QueryParams, QueryReadResult};
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::source_read::GetSourceResult;
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};
use rmcp::ServiceExt as _;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RoleServer, RunningService, ServiceError};
use serde_json::json;

use super::super::{CandidateViews, view_key};
use crate::proxy::{OutputPolicy, RiftProxy, Upstream};
use crate::server::RiftMcp;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct Fixture {
    serving: RunningService<RoleServer, RiftMcp>,
    forwarding: RunningService<RoleServer, RiftProxy>,
    client: RunningService<RoleClient, ()>,
    views: Arc<CandidateViews>,
}

impl Fixture {
    async fn start(root: &Path) -> TestResult<Self> {
        Box::pin(async move {
            crate::server::hermetic_workspace(root, "")?;
            let server =
                RiftMcp::build_settled(root, rift_index::WorkspaceIndexLimits::default()).await?;
            let (server_transport, upstream_transport) = tokio::io::duplex(64 * 1024);
            let (serving, upstream) =
                tokio::join!(server.serve(server_transport), ().serve(upstream_transport));
            let proxy = RiftProxy::new(
                root,
                crate::proxy::tests::test_identity(),
                OutputPolicy::All,
            );
            let views = Arc::clone(&proxy.candidate_views);
            {
                let mut slot = proxy.upstream.lock().await;
                slot.connected = Some(Upstream {
                    running: upstream?,
                    generation: 0,
                });
                slot.generation_next = 1;
            }
            let (proxy_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let (forwarding, client) =
                tokio::join!(proxy.serve(proxy_transport), ().serve(client_transport));
            Ok(Self {
                serving: serving?,
                forwarding: forwarding?,
                client: client?,
                views,
            })
        })
        .await
    }

    async fn view(&self) -> TestResult<CapturedView> {
        let producer = serde_json::from_value(json!({
            "query":"beacon", "target":"symbol", "scope":"all"
        }))?;
        let params: QueryParams = rift_search::sparql::retrieval_query(&producer)?;
        let response = self
            .client
            .call_tool(
                CallToolRequestParams::new("search")
                    .with_arguments(serde_json::from_value(serde_json::to_value(params)?)?),
            )
            .await?;
        assert_ne!(response.is_error, Some(true), "{response:?}");
        let result: QueryReadResult =
            serde_json::from_value(response.structured_content.ok_or_else(|| {
                format!(
                    "query response lacks structured content: {:?}",
                    response.content
                )
            })?)?;
        let QueryReadResult::Unavailable(SearchUnavailable::Unavailable {
            reason: SymbolUnavailableReason::InsufficientCoverage,
            view: Some(view),
            ..
        }) = result
        else {
            return Err(format!("expected incomplete global selection: {result:?}").into());
        };
        Ok(view)
    }

    async fn source(&self, view: &CapturedViewId) -> Result<CallToolResult, ServiceError> {
        self.client
            .call_tool(
                CallToolRequestParams::new("get_source").with_arguments(
                    serde_json::from_value(
                        json!({"unit":"rift://source/project/lib.rs", "view":view}),
                    )
                    .expect("static source request"),
                ),
            )
            .await
    }

    async fn stop(self) -> TestResult {
        self.client.cancel().await?;
        self.forwarding.cancel().await?;
        self.serving.cancel().await?;
        Ok(())
    }
}

fn refusal(result: Result<CallToolResult, ServiceError>, message: &str) -> TestResult {
    let ServiceError::McpError(error) = result.expect_err("invalid view must refuse") else {
        return Err("view refusal must retain its registered MCP error".into());
    };
    assert!(error.message.contains(message), "{error:?}");
    let data = error.data.ok_or("registered view refusal data")?;
    assert_eq!(data["code"], "invalid_request");
    assert_eq!(data["retry"], "never");
    Ok(())
}

#[tokio::test]
async fn unavailable_query_views_keep_original_source_and_refuse_foreign_or_expired_views()
-> TestResult {
    let directory = tempfile::tempdir()?;
    let original = "pub fn beacon() {}\n";
    std::fs::write(directory.path().join("lib.rs"), original)?;
    let fixture = Fixture::start(directory.path()).await?;
    let view = fixture.view().await?;
    std::fs::write(directory.path().join("lib.rs"), "pub fn changed() {}\n")?;
    let response = fixture.source(&view.id).await?;
    assert_ne!(response.is_error, Some(true), "{response:?}");
    let source: GetSourceResult =
        serde_json::from_value(response.structured_content.ok_or("source answer")?)?;
    let GetSourceResult::Found {
        source,
        view: selected,
        ..
    } = source
    else {
        return Err(format!("expected captured source: {source:?}").into());
    };
    assert_eq!(source.text, original);
    assert_eq!(selected, view);
    refusal(
        fixture
            .source(&CapturedViewId::parse("claims.forged")?)
            .await,
        "belongs to another workspace",
    )?;
    let foreign_directory = tempfile::tempdir()?;
    std::fs::write(foreign_directory.path().join("lib.rs"), original)?;
    let foreign = Fixture::start(foreign_directory.path()).await?;
    let foreign_view = foreign.view().await?;
    refusal(
        fixture.source(&foreign_view.id).await,
        "belongs to another workspace",
    )?;
    {
        let mut rows = fixture
            .views
            .rows
            .lock()
            .expect("view rows are not poisoned");
        let row = rows
            .iter_mut()
            .find(|row| row.key == view_key(view.id.as_str()))
            .ok_or("issued view mapping")?;
        row.expires = SystemTime::UNIX_EPOCH;
    }
    refusal(fixture.source(&view.id).await, "expired")?;
    foreign.stop().await?;
    fixture.stop().await
}
