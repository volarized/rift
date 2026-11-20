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
    /// The revision its pattern search names, as a served project names its own.
    revision: Option<String>,
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
        let mut retrieval = json!({
            "results": [{"hit":{"target":"symbol", "symbol":{
                "id":"rift://symbol/local/rust/app/beacon", "language":"rust", "name":"beacon", "kind":"function"
            }}, "path":"lib.rs", "source":"fn beacon() {}", "range":{"start":0,"end":14}, "line":1}],
            "pagination": {"page": 1, "pages": 1, "max_results": 10, "total": 10}
        });
        if let Some(revision) = &self.revision {
            retrieval["revision"] = json!(revision);
        }
        std::future::ready(match request.name.as_ref() {
            "search_retrieval" => Ok(CallToolResult::structured(retrieval).into()),
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
    connect_upstream(backend, RecordingUpstream::default()).await
}

/// Connects `backend` to `recording`, as [`connect_recording_upstream`] does.
async fn connect_upstream(
    backend: &RiftProxy,
    recording: RecordingUpstream,
) -> TestResult<(
    RecordingUpstream,
    RunningService<RoleServer, RecordingUpstream>,
)> {
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

/// #743: a combined query names the revision it read. A later page sent with that revision
/// pages the retained answer without asking any project again; another revision is refused.
#[tokio::test]
async fn test_a_combined_query_pins_later_pages_by_its_revision() -> TestResult {
    let primary = tempfile::tempdir()?;
    let extra = tempfile::tempdir()?;
    std::fs::write(
        primary.path().join("rift.toml"),
        "[search]\ndefault_scope = \"all\"\n",
    )?;
    let proxy = proxy_with_api(primary.path(), extra.path())?;
    let revised = |digit: &str| RecordingUpstream {
        revision: Some(format!("rift://revision/tree/{}", digit.repeat(64))),
        ..RecordingUpstream::default()
    };
    let (local, _local_connection) = connect_upstream(&proxy, revised("a")).await?;
    let (api, _api_connection) = connect_upstream(&proxy.backends["api"], revised("b")).await?;
    let paged = |page: u64, revision: Option<&str>| -> TestResult<CallToolRequestParams> {
        let mut arguments = pattern_search("")?.arguments.unwrap_or_default();
        arguments.insert("page".to_owned(), json!(page));
        arguments.insert("max_results".to_owned(), json!(1));
        if let Some(revision) = revision {
            arguments.insert("revision".to_owned(), json!(revision));
        }
        Ok(CallToolRequestParams::new("search").with_arguments(arguments))
    };
    let result = |response: CallToolResponse| -> TestResult<QueryResult> {
        let CallToolResponse::Complete(answer) = response else {
            return Err("the query answers a complete result".into());
        };
        Ok(serde_json::from_value(
            answer.structured_content.ok_or("typed query result")?,
        )?)
    };

    let first = result(proxy.query_forward(&paged(1, None)?).await??)?;
    let revision = first
        .revision
        .ok_or("a combined answer names its revision")?;
    let pinned = revision.to_string();
    assert!(pinned.starts_with("rift://revision/tree/"), "{pinned}");
    for digit in ["a", "b"] {
        assert_ne!(pinned, format!("rift://revision/tree/{}", digit.repeat(64)));
    }
    let pagination = first.pagination.ok_or("first page pagination")?;
    assert_eq!((pagination.page, pagination.pages), (1, 2));

    let second = result(proxy.query_forward(&paged(2, Some(&pinned))?).await??)?;
    assert_eq!(second.revision.as_ref(), Some(&revision));
    let pagination = second.pagination.ok_or("second page pagination")?;
    assert_eq!((pagination.page, pagination.pages), (2, 2));
    assert_ne!(first.answer, second.answer, "pages partition the answer");
    assert_eq!(
        tools(&local),
        ["search_retrieval"],
        "page 2 reads no project"
    );
    assert_eq!(tools(&api), ["search_retrieval"], "page 2 reads no project");

    let other = format!("rift://revision/tree/{}", "c".repeat(64));
    let refusal = proxy
        .query_forward(&paged(2, Some(&other))?)
        .await
        .expect_err("an unread revision is refused");
    assert_eq!(
        refusal.slug().as_str(),
        "rift.server.read_revision_unserved",
        "{refusal}"
    );
    Ok(())
}

fn retained_answer(serial: u64, bytes: usize) -> super::RetainedAnswer {
    let mut key = [0; 32];
    key[..8].copy_from_slice(&serial.to_le_bytes());
    super::RetainedAnswer {
        key,
        view: rift_protocol::symbol_read::CapturedView {
            id: rift_protocol::symbol_read::CapturedViewId::parse(&format!("proxy.answer{serial}"))
                .expect("view"),
            expires_at: "2030-01-01T00:00:00Z".to_owned(),
        },
        expires: std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(2_000_000_000),
        bytes: vec![0; bytes].into(),
    }
}

#[test]
fn query_retention_uses_configured_count_and_lru() {
    let store = super::QueryRoutes::default();
    let limits = rift_server::RetainedViewLimits::new(2, 100);
    let now = std::time::SystemTime::UNIX_EPOCH;
    let first = retained_answer(0, 4).key;
    let second = retained_answer(1, 4).key;
    store.retain(retained_answer(0, 4), limits, now);
    store.retain(retained_answer(1, 4), limits, now);
    assert!(store.retained(&first, now).is_some());
    store.retain(retained_answer(2, 4), limits, now);
    assert!(store.retained(&first, now).is_some());
    assert!(store.retained(&second, now).is_none());
    assert!(store.retained(&retained_answer(2, 4).key, now).is_some());
}

#[test]
fn query_retention_accepts_more_than_the_default_count() {
    let store = super::QueryRoutes::default();
    let limits = rift_server::RetainedViewLimits::new(257, 1 << 20);
    let now = std::time::SystemTime::UNIX_EPOCH;
    for serial in 0..257 {
        store.retain(retained_answer(serial, 1), limits, now);
    }
    assert!(store.retained(&retained_answer(0, 1).key, now).is_some());
    assert!(store.retained(&retained_answer(256, 1).key, now).is_some());
}

#[test]
fn query_retention_uses_configured_bytes_and_preserves_views_on_refusal() {
    let store = super::QueryRoutes::default();
    let limits = rift_server::RetainedViewLimits::new(256, 8);
    let now = std::time::SystemTime::UNIX_EPOCH;
    store.retain(retained_answer(0, 4), limits, now);
    store.retain(retained_answer(1, 4), limits, now);
    assert!(store.retained(&retained_answer(0, 4).key, now).is_some());
    store.retain(retained_answer(2, 4), limits, now);
    assert!(store.retained(&retained_answer(1, 4).key, now).is_none());
    store.retain(retained_answer(3, 9), limits, now);
    assert!(store.retained(&retained_answer(0, 4).key, now).is_some());
    assert!(store.retained(&retained_answer(2, 4).key, now).is_some());
    assert!(store.retained(&retained_answer(3, 9).key, now).is_none());
}

#[test]
fn query_retention_accepts_more_than_the_old_aggregate_byte_bound() {
    let store = super::QueryRoutes::default();
    let limits = rift_server::RetainedViewLimits::new(2, 64 << 20);
    let now = std::time::SystemTime::UNIX_EPOCH;
    store.retain(retained_answer(0, 9 << 20), limits, now);
    store.retain(retained_answer(1, 9 << 20), limits, now);
    assert!(store.retained(&retained_answer(0, 0).key, now).is_some());
    assert!(store.retained(&retained_answer(1, 0).key, now).is_some());
}

#[tokio::test]
async fn proxy_retention_uses_the_primary_project_configuration() -> TestResult {
    let primary = tempfile::tempdir()?;
    let extra = tempfile::tempdir()?;
    std::fs::write(
        primary.path().join("rift.toml"),
        "[search.query]\nretained_views = 2\nretained_size = '32mb'\n",
    )?;
    std::fs::write(
        extra.path().join("rift.toml"),
        "[search.query]\nretained_views = 1\nretained_size = '1mb'\n",
    )?;
    let proxy = proxy_with_api(primary.path(), extra.path())?;
    assert_eq!(
        proxy.retention_limits().await?,
        rift_server::RetainedViewLimits::new(2, 32 << 20)
    );
    Ok(())
}

#[tokio::test]
async fn combined_query_re_evaluates_a_view_released_by_configured_count() -> TestResult {
    let primary = tempfile::tempdir()?;
    let extra = tempfile::tempdir()?;
    std::fs::write(
        primary.path().join("rift.toml"),
        "[search]\ndefault_scope = 'all'\n[search.query]\nretained_views = 1\nretained_size = '1mb'\n",
    )?;
    let proxy = proxy_with_api(primary.path(), extra.path())?;
    let revised = |digit: &str| RecordingUpstream {
        revision: Some(format!("rift://revision/tree/{}", digit.repeat(64))),
        ..RecordingUpstream::default()
    };
    let (local, _local_connection) = connect_upstream(&proxy, revised("a")).await?;
    let (api, _api_connection) = connect_upstream(&proxy.backends["api"], revised("b")).await?;
    let mut first = pattern_search("")?;
    first
        .arguments
        .get_or_insert_default()
        .insert("max_results".to_owned(), json!(1));
    let CallToolResponse::Complete(answer) = proxy.query_forward(&first).await?? else {
        return Err("complete query result".into());
    };
    let result: QueryResult =
        serde_json::from_value(answer.structured_content.ok_or("typed result")?)?;
    let revision = result.revision.ok_or("combined revision")?;
    let mut other = first.clone();
    let arguments = other.arguments.get_or_insert_default();
    let statement = format!(
        "{} ORDER BY ?symbol",
        arguments["query"].as_str().ok_or("statement")?
    );
    arguments.insert("query".to_owned(), json!(statement));
    assert_eq!(binding_count(proxy.query_forward(&other).await??)?, 1);
    let arguments = first.arguments.get_or_insert_default();
    arguments.insert("page".to_owned(), json!(2));
    arguments.insert("revision".to_owned(), json!(revision));
    assert_eq!(binding_count(proxy.query_forward(&first).await??)?, 1);
    assert_eq!(tools(&local), ["search_retrieval"; 3]);
    assert_eq!(tools(&api), ["search_retrieval"; 3]);
    Ok(())
}
