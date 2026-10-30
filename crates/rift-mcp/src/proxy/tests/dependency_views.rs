use super::*;
use rift_protocol::symbol_read::{CapturedView, GetSymbolResult};
use rmcp::model::{CallToolRequestParams, CallToolResponse};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

const SYMBOL: &str = "rift://symbol/cargo/crates.io/demo@1.0.0/rust/demo/beacon";
const UNIT: &str = "rift://source/cargo/crates.io/demo@1.0.0/src/lib.rs";
const ENDPOINT: &str = "https://rift.test";

async fn answer_project(
    backend: &RiftProxy,
    project: &'static str,
    expiry: String,
) -> tokio::task::JoinHandle<std::io::Result<()>> {
    let (running, upstream) = direct_upstream();
    {
        let mut slot = backend.upstream.lock().await;
        slot.connected = Some(Upstream {
            running,
            generation: 0,
        });
        slot.generation_next = 1;
    }
    tokio::spawn(async move {
        let (reading, mut writing) = tokio::io::split(upstream);
        let mut lines = tokio::io::BufReader::new(reading).lines();
        let key = if project == "main" {
            "claims.remote".to_owned()
        } else {
            format!("claims.{project}")
        };
        let top = json!({"id":key, "expires_at":expiry});
        let remote = json!({"id":"claims.remote", "expires_at":expiry});
        for _ in 0..3 {
            let line = lines
                .next_line()
                .await?
                .ok_or(std::io::ErrorKind::UnexpectedEof)?;
            let request: Value = serde_json::from_str(&line)?;
            let params = &request["params"];
            let arguments = &params["arguments"];
            let tool = params["name"].as_str().expect("tool name");
            let result = match tool {
                "search" => {
                    if let Some(view) = arguments.get("view") {
                        assert_eq!(view, &top["id"]);
                    }
                    json!({"view":top, "results":[{"view":remote, "hit":{"target":"symbol",
                        "symbol":{"id":SYMBOL,"language":"rust","name":"beacon","kind":"function"}},
                        "unit":UNIT,"distance":1}],"pagination":{"page_index":0,"total_pages":1}})
                }
                "get_symbol" => {
                    assert_eq!(arguments["view"], remote["id"]);
                    assert_eq!(params["_meta"]["root"], ENDPOINT);
                    assert_eq!(params["_meta"]["view"], remote);
                    json!({"outcome":"missing", "symbol_not_found":{"id":SYMBOL,
                        "alternatives":[],"detail":project},"view":remote,
                        "revision":"rift://revision/cargo/crates.io/demo@1.0.0"})
                }
                _ => panic!("unexpected dependency request: {request}"),
            };
            let answer = json!({"jsonrpc":"2.0","id":request["id"],"result":{
                "content":[],"structuredContent":result,"isError":tool=="get_symbol",
                "_meta":{"backend":"global","root":ENDPOINT,"owner":null,"view":remote}
            }});
            writing.write_all(format!("{answer}\n").as_bytes()).await?;
        }
        Ok(())
    })
}

async fn read(
    proxy: &RiftProxy,
    tool: &str,
    arguments: Value,
) -> TestResult<rmcp::model::CallToolResult> {
    let request = CallToolRequestParams::new(tool.to_owned())
        .with_arguments(serde_json::from_value(arguments)?);
    let CallToolResponse::Complete(result) = Box::pin(proxy.captured_read(&request))
        .await?
        .ok_or("captured read")??
    else {
        return Err("complete captured response".into());
    };
    Ok(result)
}

fn walk(project: &str, view: Option<&Value>) -> Value {
    let owner = if project == "local" {
        "local"
    } else {
        "local@cloud"
    };
    let mut arguments = json!({"traversal":{"seed":format!("rift://symbol/{owner}/rust/app/load"),
        "direction":"outgoing"},"target":"symbol","limit":1});
    if let Some(view) = view {
        arguments["view"] = view.clone();
    }
    arguments
}

#[tokio::test]
async fn interleaved_profiles_keep_dependency_aliases_missing_and_source_views() -> TestResult {
    let primary = tempfile::tempdir()?;
    let extra = tempfile::tempdir()?;
    let proxy = named_proxy(primary.path(), extra.path())?;
    let expiry = jiff::Timestamp::now()
        .checked_add(std::time::Duration::from_secs(600))?
        .to_string();
    let main = answer_project(&proxy, "main", expiry.clone()).await;
    let cloud = answer_project(&proxy.backends["cloud"], "cloud", expiry).await;
    let mut captured = Vec::new();
    for project in ["local", "cloud"] {
        let answer = read(&proxy, "search", walk(project, None)).await?;
        let body = answer.structured_content.ok_or("walk body")?;
        let view: CapturedView = serde_json::from_value(body["results"][0]["view"].clone())?;
        assert_ne!(view.id.as_str(), "claims.remote");
        captured.push((project, body, view));
    }
    assert_ne!(captured[0].2.id, captured[1].2.id);
    for (project, _, view) in &captured {
        let result = read(&proxy, "get_symbol", json!({"id":SYMBOL,"view":view.id})).await?;
        assert_eq!(result.is_error, Some(true));
        let body = result.structured_content.ok_or("missing body")?;
        let typed: GetSymbolResult = serde_json::from_value(body.clone())?;
        assert!(
            matches!(typed, GetSymbolResult::Missing { view: held, .. } if held == *view),
            "{body}"
        );
        assert_eq!(
            body["symbol_not_found"]["detail"],
            if *project == "local" { "main" } else { "cloud" }
        );
    }
    for (project, original, view) in &captured {
        let held = read(
            &proxy,
            "search",
            walk(project, Some(&original["view"]["id"])),
        )
        .await?;
        assert_eq!(held.structured_content.as_ref(), Some(original));
        let wrong = CallToolRequestParams::new("get_symbol").with_arguments(
            serde_json::from_value(json!({
                "id":"rift://symbol/cargo/crates.io/other@1.0.0/rust/other/beacon","view":view.id
            }))?,
        );
        assert!(proxy.captured_read(&wrong).await.is_err());
    }
    main.await??;
    cloud.await??;
    assert_eq!(proxy.upstream.lock().await.generation_next, 1);
    Ok(())
}
