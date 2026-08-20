//! Real stdio MCP dogfood contract.

use std::error::Error;
use std::path::PathBuf;
use std::process::Stdio;

use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;
use rmcp::{ServiceExt as _, transport::ConfigureCommandExt as _};
use serde_json::json;
use tokio::io::AsyncReadExt as _;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn arguments(value: &serde_json::Value) -> TestResult<serde_json::Map<String, serde_json::Value>> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| "tool arguments must be an object".into())
}

#[tokio::test]
async fn rift_reads_its_own_rust_source_over_real_stdio_mcp() -> TestResult {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (transport, stderr) = TokioChildProcess::builder(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_rift")).configure(|command| {
            command.arg("mcp").current_dir(root);
        }),
    )
    .stderr(Stdio::piped())
    .spawn()?;
    let mut stderr = stderr.ok_or("MCP stderr missing")?;
    let stderr_task = tokio::spawn(async move {
        let mut output = String::new();
        stderr.read_to_string(&mut output).await?;
        Ok::<_, std::io::Error>(output)
    });

    let client = ().serve(transport).await?;
    let peer = client
        .peer_info()
        .ok_or("server peer information missing")?;
    assert_eq!(
        peer.server_info
            .as_ref()
            .ok_or("server implementation missing")?
            .name,
        "rift"
    );
    let tools = client.list_all_tools().await?;
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        ["get_symbol", "nodes", "search"]
    );

    let called = client
        .call_tool(
            CallToolRequestParams::new("get_symbol")
                .with_arguments(arguments(&json!({"name": "cli_command"}))?),
        )
        .await?;
    assert_eq!(
        called
            .structured_content
            .ok_or("get_symbol must return structured content")?["hits"][0]["symbol"]["name"],
        "cli_command"
    );

    client.cancel().await?;
    let stderr = stderr_task.await??;
    assert!(stderr.is_empty(), "stdio server wrote stderr: {stderr}");
    Ok(())
}
