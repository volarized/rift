use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rift_protocol::configuration::{McpConfiguration, ServerConfiguration};
use rift_protocol::lock::ProductIdentity;
use rift_protocol::query::{QueryAnswer, QueryResult};
use rmcp::ErrorData;
use rmcp::ServiceExt as _;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer, RunningService};
use serde_json::{Value, json};

use super::super::{RiftProxy, Upstream, extra::Registrations};
use crate::output::OutputPolicy;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// One tool call an upstream received: the tool, then its arguments.
type ReceivedCall = (String, Value);

/// An upstream that records every tool call it receives. It answers a pattern search
/// with one `beacon` hit and a forwarded query with no binding.
#[derive(Clone, Default)]
struct RecordingUpstream {
    received: Arc<Mutex<Vec<ReceivedCall>>>,
}

impl RecordingUpstream {
    fn received(&self) -> Vec<ReceivedCall> {
        self.received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl rmcp::ServerHandler for RecordingUpstream {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> {
        let arguments = Value::Object(request.arguments.clone().unwrap_or_default());
        self.received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((request.name.to_string(), arguments));
        std::future::ready(match request.name.as_ref() {
            "search_retrieval" => Ok(CallToolResult::structured(json!({
                "results": [{"hit":{"target":"symbol", "symbol":{
                    "id":"rift://symbol/local/rust/app/beacon", "language":"rust", "name":"beacon", "kind":"function"
                }}, "path":"lib.rs", "source":"fn beacon() {}", "range":{"start":0,"end":14}, "line":1}],
                "pagination": {"page": 1, "pages": 1, "max_results": 10, "total": 10}
            }))
            .into()),
            "search" => Ok(CallToolResult::structured(json!({
                "answer": {"result":"bindings", "variables":["symbol"], "bindings":[]},
                "pagination": {"page": 1, "pages": 0, "max_results": 10, "total": 0}
            }))
            .into()),
            other => Err(ErrorData::invalid_params(
                format!("unknown tool {other}"),
                None,
            )),
        })
    }
}

/// A proxy for `root` that registered `extra_root` as `api`.
fn proxy_with_api(root: &Path, extra_root: &Path) -> TestResult<RiftProxy> {
    let mut saved = McpConfiguration::default();
    saved
        .extra
        .insert("api".to_owned(), extra_root.to_string_lossy().into_owned());
    let extra = Registrations::accept(root, &saved, &[], None)?;
    let identity = ProductIdentity {
        version: "0.0.11+b006b8433ba06679f06a3c7f0743d65634d32c34".to_owned(),
        schema_digest: "c".repeat(64),
    };
    Ok(RiftProxy::with_configuration(
        root,
        identity,
        OutputPolicy::All,
        &ServerConfiguration::default(),
        extra,
    ))
}

/// Connects `backend` to a [`RecordingUpstream`] and returns that upstream with its
/// connection, which the caller holds for the life of the test.
async fn connect_recording_upstream(
    backend: &RiftProxy,
) -> TestResult<(
    RecordingUpstream,
    RunningService<RoleServer, RecordingUpstream>,
)> {
    let recording = RecordingUpstream::default();
    let (client_half, server_half) = tokio::io::duplex(64 * 1024);
    let (running, upstream) = tokio::join!(
        Box::pin(().serve(client_half)),
        Box::pin(recording.clone().serve(server_half))
    );
    let mut slot = backend.upstream.lock().await;
    slot.connected = Some(Upstream {
        running: running?,
        generation: 0,
    });
    slot.generation_next = 1;
    Ok((recording, upstream?))
}

/// A pattern search for `beacon` whose service carries `scope_input`, such as
/// `:scope 'local';`, or nothing.
fn pattern_search(scope_input: &str) -> TestResult<CallToolRequestParams> {
    let statement = format!(
        "SELECT ?symbol {{ SERVICE :search {{ ?hit :pattern 'beacon'; {scope_input} :target 'symbol'; :symbol ?symbol }} }}"
    );
    Ok(CallToolRequestParams::new("search")
        .with_arguments(serde_json::from_value(json!({"query": statement}))?))
}

/// The bindings one completed query answer carries.
fn binding_count(response: CallToolResponse) -> TestResult<usize> {
    let CallToolResponse::Complete(answer) = response else {
        return Err("the query answers a complete result".into());
    };
    let result: QueryResult =
        serde_json::from_value(answer.structured_content.ok_or("typed query result")?)?;
    let QueryAnswer::Bindings { bindings, .. } = result.answer else {
        return Err("the query answers bindings".into());
    };
    Ok(bindings.len())
}

/// The scope each `search_retrieval` call an upstream received names.
fn retrieval_scopes(upstream: &RecordingUpstream) -> Vec<Value> {
    upstream
        .received()
        .into_iter()
        .filter(|(tool, _)| tool == "search_retrieval")
        .map(|(_, arguments)| arguments["scope"].clone())
        .collect()
}

/// The tools an upstream was called with, in arrival order.
fn tools(upstream: &RecordingUpstream) -> Vec<String> {
    upstream
        .received()
        .into_iter()
        .map(|(tool, _)| tool)
        .collect()
}

/// `[search] default_scope = "all"` in the primary project's `rift.toml` sends a service
/// naming no `:scope` to every accepted project: the primary reads `all` and the extra
/// project reads its own tree.
#[tokio::test]
async fn test_default_scope_all_routes_an_unnamed_scope_to_every_accepted_project() -> TestResult {
    let primary = tempfile::tempdir()?;
    let extra = tempfile::tempdir()?;
    std::fs::write(
        primary.path().join("rift.toml"),
        "[search]\ndefault_scope = \"all\"\n",
    )?;
    let proxy = proxy_with_api(primary.path(), extra.path())?;
    let (local, _local_connection) = connect_recording_upstream(&proxy).await?;
    let (api, _api_connection) = connect_recording_upstream(&proxy.backends["api"]).await?;

    let answer = proxy.query_forward(&pattern_search("")?).await??;

    assert_eq!(binding_count(answer)?, 2, "one hit from each project");
    assert_eq!(tools(&local), ["search_retrieval"]);
    assert_eq!(retrieval_scopes(&local), [json!("all")]);
    assert_eq!(tools(&api), ["search_retrieval"]);
    assert_eq!(retrieval_scopes(&api), [json!("local")]);
    Ok(())
}

/// A service naming `:scope` keeps it under a configured default: the proxy forwards the
/// statement to the primary project unchanged and asks no other project.
#[tokio::test]
async fn test_default_scope_all_leaves_a_named_scope_with_the_primary_project() -> TestResult {
    let primary = tempfile::tempdir()?;
    let extra = tempfile::tempdir()?;
    std::fs::write(
        primary.path().join("rift.toml"),
        "[search]\ndefault_scope = \"all\"\n",
    )?;
    let proxy = proxy_with_api(primary.path(), extra.path())?;
    let (local, _local_connection) = connect_recording_upstream(&proxy).await?;
    let (api, _api_connection) = connect_recording_upstream(&proxy.backends["api"]).await?;
    let request = pattern_search(":scope 'local';")?;

    let answer = proxy.query_forward(&request).await??;

    assert_eq!(binding_count(answer)?, 0);
    let forwarded = Value::Object(request.arguments.clone().unwrap_or_default());
    assert_eq!(local.received(), [("search".to_owned(), forwarded)]);
    assert!(api.received().is_empty(), "{:?}", api.received());
    Ok(())
}

/// With no `default_scope` configured, a service naming no `:scope` reads the primary
/// project alone.
#[tokio::test]
async fn test_an_unnamed_scope_stays_with_the_primary_project_by_default() -> TestResult {
    let primary = tempfile::tempdir()?;
    let extra = tempfile::tempdir()?;
    let proxy = proxy_with_api(primary.path(), extra.path())?;
    let (local, _local_connection) = connect_recording_upstream(&proxy).await?;
    let (api, _api_connection) = connect_recording_upstream(&proxy.backends["api"]).await?;

    let answer = proxy.query_forward(&pattern_search("")?).await??;

    assert_eq!(binding_count(answer)?, 0);
    assert_eq!(tools(&local), ["search"]);
    assert!(api.received().is_empty(), "{:?}", api.received());
    Ok(())
}
