use std::path::{Path, PathBuf};
use std::sync::RwLock;

use rift_core::{ErrorName, RetryDirective};
use rift_index::WorkspaceIndexLimits;
use rift_protocol::change::{ChangeResult, InsertSymbolParams, ReplaceSymbolParams};
use rift_protocol::error as wire;
use rift_protocol::read::{
    GetSymbolParams, GetSymbolResult, NodesParams, NodesResult, SearchParams, SearchResult,
};
use rift_server::{ChangeService, ReadError, ReadService};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{ErrorCode, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, Json, ServerHandler, tool, tool_handler, tool_router};

/// JSON-RPC error code every Rift operating failure travels under. The
/// machine-readable classification is the [`wire::ErrorData`] in `data`.
const RIFT_ERROR_CODE: ErrorCode = ErrorCode(-32000);

/// Most `causes` entries one wire error carries, matching the advertised
/// schema bound.
const ERROR_CAUSES_MAX: usize = 8;

/// Rust workspace MCP server: reads serve an immutable snapshot, changes
/// write the workspace and swap in a fresh snapshot.
#[derive(Debug)]
pub struct RiftMcp {
    root: PathBuf,
    limits: WorkspaceIndexLimits,
    reads: RwLock<ReadService>,
    changes: ChangeService,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router, vis = "pub(crate)")]
impl RiftMcp {
    /// Builds server from one direct-workspace snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when workspace cannot be indexed within bounds.
    pub fn build(root: &Path, limits: WorkspaceIndexLimits) -> Result<Self, ReadError> {
        Ok(Self {
            root: root.to_path_buf(),
            limits,
            reads: RwLock::new(ReadService::build(root, limits)?),
            changes: ChangeService::new(root),
            tool_router: Self::tool_router(),
        })
    }

    /// Finds Rust declarations and their source by exact symbol name. Each hit
    /// carries the declaration and its source excerpt; `include_body: false` omits
    /// both. Use `search` when the name is not exactly known.
    #[tool]
    fn get_symbol(
        &self,
        Parameters(params): Parameters<GetSymbolParams>,
    ) -> Result<Json<GetSymbolResult>, ErrorData> {
        self.snapshot()
            .get_symbol(&params)
            .map(Json)
            .map_err(|error| tool_error(&error))
    }

    /// Searches indexed Rust declarations and source lines by lexical `query`. Use
    /// `get_symbol` when the declaration name is known.
    #[tool]
    fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<Json<SearchResult>, ErrorData> {
        self.snapshot()
            .search(&params)
            .map(Json)
            .map_err(|error| tool_error(&error))
    }

    /// Lists the syntax nodes covering one UTF-8 byte position in one file,
    /// outermost first. Each identity carries a witness, so an address taken
    /// from this listing refuses cleanly once the file's bytes drift.
    #[tool]
    fn nodes(
        &self,
        Parameters(params): Parameters<NodesParams>,
    ) -> Result<Json<NodesResult>, ErrorData> {
        self.snapshot()
            .nodes(params)
            .map(Json)
            .map_err(|error| tool_error(&error))
    }

    /// Replaces one declaration addressed by symbol. The parser derives the
    /// span, so the caller supplies no offsets; a refusal names the failed
    /// precondition and leaves the workspace untouched.
    #[tool]
    fn replace_symbol(
        &self,
        Parameters(params): Parameters<ReplaceSymbolParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.change(|reads, changes| changes.replace_symbol(reads, &params))
    }

    /// Inserts a new declaration before or after an existing one, addressed
    /// by its anchor symbol. A refusal names the failed precondition and
    /// leaves the workspace untouched.
    #[tool]
    fn insert_symbol(
        &self,
        Parameters(params): Parameters<InsertSymbolParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.change(|reads, changes| changes.insert_symbol(reads, &params))
    }

    /// Takes the current read snapshot.
    ///
    /// # Panics
    ///
    /// Recovers a poisoned lock instead of panicking: the snapshot is
    /// replaced whole, so a poisoned guard still holds a coherent value.
    fn snapshot(&self) -> std::sync::RwLockReadGuard<'_, ReadService> {
        self.reads
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Runs one change against the current snapshot and, when it lands,
    /// swaps in a snapshot of the changed workspace.
    ///
    /// A rebuild failure after a landed change rides the result as a
    /// diagnostic rather than failing the call: the write happened, and the
    /// caller must not be told otherwise.
    fn change(
        &self,
        operation: impl FnOnce(&ReadService, &ChangeService) -> Result<ChangeResult, ReadError>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        let mut result = operation(&self.snapshot(), &self.changes)
            .map_err(|error| tool_error_in(&error, wire::ErrorPhase::Change))?;
        if let ChangeResult::Applied { summary } = &mut result {
            match ReadService::build(&self.root, self.limits) {
                Ok(rebuilt) => {
                    *self
                        .reads
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = rebuilt;
                }
                Err(error) => summary.diagnostics.push(stale_snapshot_diagnostic(&error)),
            }
        }
        Ok(Json(result))
    }
}

/// The finding an applied change carries when the follow-up snapshot could
/// not be rebuilt: reads keep serving the pre-change snapshot until one can.
fn stale_snapshot_diagnostic(error: &ReadError) -> rift_protocol::read::Diagnostic {
    rift_protocol::read::Diagnostic {
        severity: rift_protocol::read::Severity::Warning,
        code: None,
        message: format!(
            "the change landed, and the read snapshot could not refresh; \
             reads serve the pre-change tree until the workspace indexes again: {error}"
        ),
        span: None,
        related: Vec::new(),
        tags: Vec::new(),
        reliability: rift_protocol::read::DiagnosticReliability::Reliable,
        continuation: rift_protocol::read::DiagnosticContinuation::Unknown,
        extensions: rift_protocol::read::Extensions(std::collections::BTreeMap::new()),
        language: None,
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RiftMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rift", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Read and edit the current workspace: get_symbol and search find \
                 declarations, nodes lists witnessed syntax nodes at a byte position, \
                 replace_symbol and insert_symbol change declarations atomically.",
            )
    }
}

/// Maps a read failure to the JSON-RPC error object the design documents:
/// code `-32000`, the rendered failure line as `message`, and the typed
/// [`wire::ErrorData`] as `data`.
fn tool_error(error: &ReadError) -> ErrorData {
    tool_error_in(error, wire::ErrorPhase::Read)
}

/// Maps one failure to the wire error object, naming the phase it stopped in.
fn tool_error_in(error: &ReadError, phase: wire::ErrorPhase) -> ErrorData {
    let message = error.to_string();
    let data = serde_json::to_value(wire_error(error, phase)).ok();
    ErrorData::new(RIFT_ERROR_CODE, message, data)
}

/// Builds the wire error payload for one read failure.
fn wire_error(error: &ReadError, phase: wire::ErrorPhase) -> wire::ErrorData {
    let descriptor = error.descriptor();
    wire::ErrorData {
        code: wire_code(descriptor.name()),
        message: error.to_string(),
        retry: wire_retry(descriptor.retry()),
        phase,
        diagnostics: Vec::new(),
        limit: None,
        causes: wire_causes(error),
    }
}

/// Maps a registry identity to its wire code. CLI-only identities never
/// reach this boundary; they classify as `internal_error` if one ever does.
fn wire_code(name: ErrorName) -> wire::ErrorCode {
    match name {
        ErrorName::InvalidRequest => wire::ErrorCode::InvalidRequest,
        ErrorName::PermissionDenied => wire::ErrorCode::PermissionDenied,
        ErrorName::ResourceNotFound => wire::ErrorCode::ResourceNotFound,
        ErrorName::ContentUnavailable => wire::ErrorCode::ContentUnavailable,
        ErrorName::CursorInvalid => wire::ErrorCode::CursorInvalid,
        ErrorName::CursorExpired => wire::ErrorCode::CursorExpired,
        ErrorName::Cancelled => wire::ErrorCode::Cancelled,
        ErrorName::LimitExceeded => wire::ErrorCode::LimitExceeded,
        ErrorName::StorageFailure => wire::ErrorCode::StorageFailure,
        ErrorName::UnsupportedPath => wire::ErrorCode::UnsupportedPath,
        ErrorName::TemporarilyUnavailable => wire::ErrorCode::TemporarilyUnavailable,
        ErrorName::ConfigurationInvalid => wire::ErrorCode::ConfigurationInvalid,
        ErrorName::CapabilityUnavailable => wire::ErrorCode::CapabilityUnavailable,
        ErrorName::InternalError
        | ErrorName::UpdateBinaryInvalid
        | ErrorName::UpdateReleaseInvalid
        | ErrorName::UpdateDownloadFailed
        | ErrorName::UpdateStagingFailed
        | ErrorName::UpdateChecksumMismatch
        | ErrorName::UpdateArchiveInvalid
        | ErrorName::UpdatePublishFailed
        | ErrorName::UpdateRollbackFailed
        | ErrorName::ArtifactStale => wire::ErrorCode::InternalError,
    }
}

/// Maps registry retry guidance to the wire directive.
fn wire_retry(retry: RetryDirective) -> wire::RetryDirective {
    match retry {
        RetryDirective::Never => wire::RetryDirective::Never,
        RetryDirective::SameRequest => wire::RetryDirective::SameRequest,
        RetryDirective::OperatorAction => wire::RetryDirective::OperatorAction,
    }
}

/// Walks the failure's source chain into bounded `causes` entries, outermost
/// first. Each level inherits the outer classification, which the read error
/// already resolved through the concrete failure it wraps.
fn wire_causes(error: &ReadError) -> Vec<wire::ErrorCause> {
    let descriptor = error.descriptor();
    let code = wire_code(descriptor.name());
    let retry = wire_retry(descriptor.retry());
    let mut causes = Vec::new();
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if causes.len() == ERROR_CAUSES_MAX {
            break;
        }
        causes.push(wire::ErrorCause {
            code,
            message: current.to_string(),
            retry,
        });
        source = current.source();
    }
    causes
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use rift_index::WorkspaceIndexLimits;
    use rift_server::ReadErrorKind;
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

    #[test]
    fn build_propagates_workspace_index_failure() {
        let error = RiftMcp::build(
            std::path::Path::new("not-a-real-rift-workspace"),
            WorkspaceIndexLimits::default(),
        )
        .expect_err("missing root must fail");
        assert_eq!(error.kind(), ReadErrorKind::Index);
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
            [
                "get_symbol",
                "insert_symbol",
                "nodes",
                "replace_symbol",
                "search"
            ]
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

        let nodes = client
            .call_tool(
                CallToolRequestParams::new("nodes")
                    .with_arguments(arguments(&json!({"path": "lib.rs", "position": 8}))?),
            )
            .await?;
        let structured = nodes
            .structured_content
            .ok_or("nodes must return structured content")?;
        let listed = structured["nodes"]
            .as_array()
            .ok_or("nodes must be an array")?;
        assert!(
            !listed.is_empty(),
            "position 8 sits inside `pub fn beacon`, so at least one node covers it"
        );
        let witness_suffix = has_witness_fragment(listed[0]["id"].as_str().unwrap_or_default());
        assert!(
            witness_suffix,
            "every listed node id must end in an eight-hex-character witness: {}",
            listed[0]["id"]
        );

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    /// Reports whether a node id ends in `#` plus eight lowercase hex digits.
    fn has_witness_fragment(id: &str) -> bool {
        id.rsplit_once('#').is_some_and(|(_, witness)| {
            witness.len() == 8
                && witness
                    .chars()
                    .all(|character| character.is_ascii_hexdigit() && !character.is_uppercase())
        })
    }

    #[tokio::test]
    async fn exported_schema_document_matches_served_tools() -> TestResult {
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
        let mut advertised = client.list_all_tools().await?;
        advertised.sort_by(|left, right| left.name.cmp(&right.name));

        let document: serde_json::Value = serde_json::from_str(&crate::schema::schema_document())?;
        let exported = document["tools"]
            .as_array()
            .ok_or("exported document must carry a tools array")?;

        assert_eq!(exported.len(), advertised.len());
        for (entry, tool) in exported.iter().zip(&advertised) {
            assert_eq!(entry["name"], json!(tool.name));
            assert_eq!(entry["description"], json!(tool.description));
            assert_eq!(entry["input_schema"], json!(tool.input_schema));
            assert_eq!(entry["output_schema"], json!(tool.output_schema));
        }

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    #[tokio::test]
    async fn client_change_lands_and_reads_serve_the_new_snapshot() -> TestResult {
        let (_directory, server) = fixture()?;
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;

        let change = client
            .call_tool(
                CallToolRequestParams::new("replace_symbol").with_arguments(arguments(&json!({
                    "symbol": "rift://symbol/rust/lib.rs/beacon",
                    "body": "pub fn beacon() -> u8 {\n    7\n}"
                }))?),
            )
            .await?;
        let structured = change
            .structured_content
            .ok_or("replace_symbol must return structured content")?;
        assert_eq!(structured["status"], json!("applied"));
        assert_eq!(structured["summary"]["paths"], json!(["lib.rs"]));

        let symbol = client
            .call_tool(
                CallToolRequestParams::new("get_symbol")
                    .with_arguments(arguments(&json!({"name": "beacon"}))?),
            )
            .await?;
        let structured = symbol
            .structured_content
            .ok_or("get_symbol must return structured content")?;
        let excerpt = structured["hits"][0]["source"]["text"]
            .as_str()
            .ok_or("hit must carry source text")?;
        assert!(
            excerpt.contains("-> u8"),
            "reads after an applied change must serve the new snapshot: {excerpt}"
        );

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    /// Calls one tool expecting a Rift wire error and returns the JSON-RPC
    /// error object.
    async fn failing_call(
        arguments_value: &serde_json::Value,
        tool: &'static str,
    ) -> TestResult<rmcp::ErrorData> {
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
            .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments(arguments_value)?))
            .await
            .expect_err("the request must be rejected");
        client.cancel().await?;
        server_task.await?;
        let ServiceError::McpError(data) = error else {
            panic!("expected protocol-level McpError, got {error:?}");
        };
        Ok(data)
    }

    #[tokio::test]
    async fn client_rejects_empty_search_query_with_typed_wire_error() -> TestResult {
        let data = failing_call(&json!({"query": ""}), "search").await?;
        assert_eq!(data.code, ErrorCode(-32000));
        assert_eq!(
            data.message.as_ref(),
            "the request does not match the documented form: field query, \
             violation empty; correct the reported field and resend the request"
        );
        let wire = data.data.ok_or("wire error data must be present")?;
        assert_eq!(wire["code"], json!("invalid_request"));
        assert_eq!(wire["retry"], json!("never"));
        assert_eq!(wire["phase"], json!("read"));
        assert_eq!(wire["causes"], json!([]));
        Ok(())
    }

    #[tokio::test]
    async fn client_rejects_zero_result_limit_as_invalid_request() -> TestResult {
        let data = failing_call(&json!({"name": "beacon", "limit": 0}), "get_symbol").await?;
        assert_eq!(data.code, ErrorCode(-32000));
        assert_eq!(
            data.message.as_ref(),
            "the request does not match the documented form: field limit, \
             violation zero; correct the reported field and resend the request"
        );
        let wire = data.data.ok_or("wire error data must be present")?;
        assert_eq!(wire["code"], json!("invalid_request"));
        Ok(())
    }
}
