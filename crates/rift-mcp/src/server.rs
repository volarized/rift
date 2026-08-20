use std::path::Path;

use rift_index::WorkspaceIndexLimits;
use rift_protocol::read::{GetSymbolParams, GetSymbolResult, SearchParams, SearchResult};
use rift_server::{ReadError, ReadErrorKind, ReadService};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, Json, ServerHandler, tool, tool_handler, tool_router};

/// Read-only Rust workspace MCP server.
#[derive(Debug)]
pub struct RiftMcp {
    reads: ReadService,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl RiftMcp {
    /// Builds server from one immutable direct-workspace snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when workspace cannot be indexed within bounds.
    pub fn build(root: &Path, limits: WorkspaceIndexLimits) -> Result<Self, ReadError> {
        Ok(Self {
            reads: ReadService::build(root, limits)?,
            tool_router: Self::tool_router(),
        })
    }

    #[tool(
        name = "get_symbol",
        description = "Finds Rust declarations and their source by exact symbol name. Each hit \
            carries the declaration and its source excerpt; `include_body: false` omits both. \
            Use `search` when the name is not exactly known."
    )]
    fn get_symbol(
        &self,
        Parameters(params): Parameters<GetSymbolParams>,
    ) -> Result<Json<GetSymbolResult>, ErrorData> {
        self.reads.get_symbol(&params).map(Json).map_err(tool_error)
    }

    #[tool(
        name = "search",
        description = "Searches indexed Rust declarations and source lines by lexical `query`. \
            Use `get_symbol` when the declaration name is known."
    )]
    fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<Json<SearchResult>, ErrorData> {
        self.reads.search(&params).map(Json).map_err(tool_error)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RiftMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rift", env!("CARGO_PKG_VERSION")))
            .with_instructions("Read current workspace with get_symbol and search.")
    }
}

fn tool_error(error: ReadError) -> ErrorData {
    let kind = error.kind();
    let message = error.to_string();
    drop(error);
    match kind {
        ReadErrorKind::Index => ErrorData::internal_error(message, None),
        ReadErrorKind::Unsupported | ReadErrorKind::Invalid | ReadErrorKind::NotFound => {
            ErrorData::invalid_params(message, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::contracts::TOOL_CONTRACTS;
    use rmcp::ServiceError;
    use rmcp::ServiceExt as _;
    use rmcp::model::{CallToolRequestParams, ErrorCode};
    use serde_json::json;

    use super::RiftMcp;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn fixture() -> TestResult<(tempfile::TempDir, RiftMcp)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default())?;
        Ok((directory, server))
    }

    fn arguments(
        value: &serde_json::Value,
    ) -> TestResult<serde_json::Map<String, serde_json::Value>> {
        value
            .as_object()
            .cloned()
            .ok_or_else(|| "tool arguments must be an object".into())
    }

    #[tokio::test]
    async fn client_lists_and_calls_exact_read_only_surface() -> TestResult {
        let (_directory, server) = fixture()?;
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;
        let tools = client.list_all_tools().await?;

        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            ["get_symbol", "search"]
        );
        assert!(tools.iter().all(|tool| tool.output_schema.is_some()));

        let symbol = client
            .call_tool(
                CallToolRequestParams::new("get_symbol")
                    .with_arguments(arguments(&json!({"name": "beacon"}))?),
            )
            .await?;
        let structured = symbol
            .structured_content
            .ok_or("get_symbol must return structured content")?;
        assert_eq!(structured["hits"][0]["symbol"]["name"], "beacon");
        assert_eq!(structured["next_cursor"], serde_json::Value::Null);

        let search = client
            .call_tool(
                CallToolRequestParams::new("search")
                    .with_arguments(arguments(&json!({"query": "beacon"}))?),
            )
            .await?;
        assert!(
            !search
                .structured_content
                .ok_or("search must return structured content")?["results"]
                .as_array()
                .ok_or("search results must be an array")?
                .is_empty()
        );

        let absent = client
            .call_tool(CallToolRequestParams::new("nodes"))
            .await
            .expect_err("unadvertised nodes tool must be absent");
        assert!(absent.to_string().contains("tool not found"));

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    #[tokio::test]
    async fn client_sees_contract_tool_descriptions() -> TestResult {
        let (_directory, server) = fixture()?;
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;
        let tools = client.list_all_tools().await?;

        for contract in TOOL_CONTRACTS {
            let tool = tools
                .iter()
                .find(|tool| tool.name.as_ref() == contract.name)
                .ok_or_else(|| format!("tool {} must be advertised", contract.name))?;
            assert_eq!(
                tool.description.as_deref(),
                Some(contract.description),
                "advertised description must match the protocol contract for {}",
                contract.name,
            );
        }

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    #[tokio::test]
    async fn client_rejects_empty_search_query_with_invalid_params() -> TestResult {
        let (_directory, server) = fixture()?;
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;

        let error = client
            .call_tool(
                CallToolRequestParams::new("search")
                    .with_arguments(arguments(&json!({"query": ""}))?),
            )
            .await
            .expect_err("empty search query must be rejected");
        let ServiceError::McpError(data) = error else {
            panic!("expected protocol-level McpError, got {error:?}");
        };
        assert_eq!(data.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(data.message.as_ref(), "search query is empty");

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    #[tokio::test]
    async fn client_rejects_zero_result_limit_with_internal_error() -> TestResult {
        let (_directory, server) = fixture()?;
        let (server_transport, client_transport) = tokio::io::duplex(16 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;

        let error = client
            .call_tool(
                CallToolRequestParams::new("get_symbol")
                    .with_arguments(arguments(&json!({"name": "beacon", "limit": 0}))?),
            )
            .await
            .expect_err("zero result limit must be rejected");
        let ServiceError::McpError(data) = error else {
            panic!("expected protocol-level McpError, got {error:?}");
        };
        assert_eq!(data.code, ErrorCode::INTERNAL_ERROR);
        assert_eq!(data.message.as_ref(), "workspace indexing failed");

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }
}
