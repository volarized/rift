use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use rift_protocol::query::{QueryAnswer, QueryParams, QueryReadResult, RdfTerm};
use rift_protocol::search_read::SearchUnavailable;
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
        let view = oxigraph::model::Literal::from(view.as_str());
        self.client
            .call_tool(
                CallToolRequestParams::new("search").with_arguments(
                    serde_json::from_value(json!({
                        "query": format!("SELECT ?outcome ?unit ?text ?start ?end ?complete ?source_view ?source_expiry {{ SERVICE :source {{ ?read :unit <rift://source/project/lib.rs>; :view {view}; :outcome ?outcome; :source [ :text ?text; :span [ :unit ?unit; :range [ :start ?start; :end ?end ] ] ]; :source_complete ?complete; :view [ :id ?source_view; :expires_at ?source_expiry ] }} }}"),
                        "language": "sparql"
                    }))
                    .expect("static Source SERVICE query"),
                ),
            )
            .await
    }

    fn record_foreign_view(
        &self,
        current: &CapturedView,
        foreign: &Self,
        view: &CapturedView,
    ) -> TestResult {
        let now = SystemTime::now();
        let selected = foreign
            .views
            .backend(&view.id, now)?
            .map_err(|reason| format!("foreign captured backend refuses: {reason:?}"))?
            .ok_or("foreign captured backend")?;
        let original = self
            .views
            .backend(&current.id, now)?
            .map_err(|reason| format!("original captured backend refuses: {reason:?}"))?
            .ok_or("original captured backend")?;
        assert_eq!(selected.view, *view);
        assert_eq!(original.view, *current);
        assert_ne!(selected.root, original.root);
        let mut pending = self.views.reserve_backend(
            &selected.backend,
            &selected.root,
            selected.owner.as_ref(),
            now,
        )?;
        pending.finalize(&selected, SystemTime::now())?;
        Ok(())
    }

    async fn stop(self) -> TestResult {
        self.client.cancel().await?;
        self.forwarding.cancel().await?;
        self.serving.cancel().await?;
        Ok(())
    }
}

fn unrecorded_refusal(result: &CallToolResult, view: &CapturedViewId) -> TestResult {
    assert_eq!(result.is_error, Some(true), "{result:?}");
    assert!(result.structured_content.is_none(), "{result:?}");
    let [content] = result.content.as_slice() else {
        return Err("one registered view refusal".into());
    };
    let text = content.as_text().ok_or("text view refusal")?;
    let view = view.as_str();
    assert_eq!(
        text.text,
        format!(
            "1 error\n\tinvalid_request · retry never\n\t\tcaptured view {view} expired or was removed: operation get_source; capture a new view and resend the request with its key\n\t\trift.server.search_view_expired\n"
        )
    );
    Ok(())
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
    let result: QueryReadResult =
        serde_json::from_value(response.structured_content.ok_or("source query answer")?)?;
    let QueryReadResult::Results(result) = result else {
        return Err(format!("expected captured Source query: {result:?}").into());
    };
    let QueryAnswer::Bindings { bindings, .. } = &result.answer else {
        return Err("Source SERVICE returns SELECT bindings".into());
    };
    let [row] = bindings.as_slice() else {
        return Err("one captured Source result".into());
    };
    let expected_end = original.len().to_string();
    for (field, expected) in [
        ("outcome", "found"),
        ("text", original),
        ("start", "0"),
        ("end", expected_end.as_str()),
        ("complete", "true"),
        ("source_view", view.id.as_str()),
        ("source_expiry", view.expires_at.as_str()),
    ] {
        let Some(RdfTerm::Literal { value, .. }) = row.get(field) else {
            return Err(format!("Source {field} must remain a typed literal: {row:?}").into());
        };
        assert_eq!(value, expected);
    }
    assert_eq!(
        row.get("unit"),
        Some(&RdfTerm::Iri {
            value: "rift://source/project/lib.rs".to_owned()
        })
    );
    let outer = result.view.ok_or("Source query outer view")?;
    let outer_expiry: jiff::Timestamp = outer.expires_at.parse()?;
    let selected_expiry: jiff::Timestamp = view.expires_at.parse()?;
    assert!(outer_expiry <= selected_expiry);
    let forged = CapturedViewId::parse("claims.forged")?;
    unrecorded_refusal(&fixture.source(&forged).await?, &forged)?;
    let foreign_directory = tempfile::tempdir()?;
    std::fs::write(foreign_directory.path().join("lib.rs"), original)?;
    let foreign = Fixture::start(foreign_directory.path()).await?;
    let foreign_view = foreign.view().await?;
    fixture.record_foreign_view(&view, &foreign, &foreign_view)?;
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
