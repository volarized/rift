use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use rift_core::constants::WORKSPACE_CONFIGURATION_FILE;
use rift_core::{ErrorName, Fault, SourceVisibility};
use rift_index::WorkspaceIndexLimits;
use rift_protocol::change::{
    ChangeResult, ChangeSummary, GuaranteeEvidence, InsertSymbolParams, PatchParams,
    ReplaceNodeParams, ReplaceSymbolParams,
};
use rift_protocol::configuration::{CommandHook, WorkspaceConfiguration};
use rift_protocol::error as wire;
use rift_protocol::read::{
    DiagnosticCode, GetSymbolParams, GetSymbolResult, NodesParams, NodesResult, SearchParams,
    SearchResult,
};
use rift_server::{
    ChangeService, ConfigurationError, HookRun, HookStatus, ReadError, ReadFault, ReadService,
    load_configuration, run_hooks,
};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{ErrorCode, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::{RwLock, Semaphore};

/// JSON-RPC error code every Rift operating failure travels under: the
/// first code of the server-defined range (-32000 to -32099), which rmcp
/// exports no constant for — its constants name only MCP-defined codes. The
/// machine-readable classification is the [`wire::ErrorData`] in `data`.
const RIFT_ERROR_CODE: ErrorCode = ErrorCode(-32000);

/// Most `causes` entries one wire error carries, matching the advertised
/// schema bound.
const ERROR_CAUSES_MAX: usize = 8;

/// Blocking filesystem and syntax operations admitted across MCP servers.
const BLOCKING_OPERATIONS_MAX: usize = 4;

/// Workspace blocking operations admitted process-wide.
fn blocking_operations() -> &'static Arc<Semaphore> {
    static OPERATIONS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    OPERATIONS.get_or_init(|| Arc::new(Semaphore::new(BLOCKING_OPERATIONS_MAX)))
}

/// Runs one blocking operation under process-wide bounded admission.
async fn bounded_blocking<Output>(
    operation: &'static str,
    work: impl FnOnce() -> Result<Output, ReadError> + Send + 'static,
) -> Result<Output, ReadError>
where
    Output: Send + 'static,
{
    let permit = Arc::clone(blocking_operations())
        .acquire_owned()
        .await
        .map_err(|error| ReadFault::task(operation, error.to_string()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|error| ReadFault::task(operation, error.to_string()))?
}

/// Rust workspace MCP server: reads serve an immutable snapshot, changes
/// write the workspace and swap in a fresh snapshot.
#[derive(Debug)]
pub struct RiftMcp {
    root: PathBuf,
    limits: WorkspaceIndexLimits,
    published: Arc<RwLock<Arc<PublishedWorkspace>>>,
    changes: Arc<ChangeService>,
    change_lane: Arc<Mutex<()>>,
    tool_router: ToolRouter<Self>,
}

/// Read index and configuration policy published as one immutable value.
#[derive(Debug)]
struct PublishedWorkspace {
    reads: Arc<ReadService>,
    configuration: ConfigurationState,
}

/// The last admission of the workspace's `rift.toml`, kept with the file
/// state it was read from so an edited file is re-admitted on the next
/// request and an unchanged one is not re-parsed per call.
#[derive(Debug, Clone)]
struct ConfigurationState {
    admitted: Result<WorkspaceConfiguration, Arc<ConfigurationError>>,
    fingerprint: Option<ConfigurationFingerprint>,
}

impl ConfigurationState {
    /// Admits the workspace's current `rift.toml`.
    fn admit(root: &Path) -> Self {
        Self {
            admitted: load_configuration(root).map_err(Arc::new),
            fingerprint: configuration_fingerprint(root),
        }
    }

    /// The admission's outcome as one request sees it: the configuration to
    /// serve under, or the typed refusal naming what to fix.
    fn admitted(&self, phase: wire::ErrorPhase) -> Result<WorkspaceConfiguration, ErrorData> {
        match &self.admitted {
            Ok(configuration) => Ok(configuration.clone()),
            Err(error) => Err(error.tool_error(phase)),
        }
    }

    /// The `[source]` policy from the last admission, or the default policy
    /// while `rift.toml` is invalid.
    fn source_visibility(&self) -> SourceVisibility {
        self.admitted.as_ref().map_or_else(
            |_| SourceVisibility::default(),
            |configuration| SourceVisibility::from(&configuration.source),
        )
    }
}

/// The file state one admission was read from. Size rides modification
/// time because same-second edits are common at a shell; an edit that
/// preserves both is not re-admitted until either moves.
type ConfigurationFingerprint = (SystemTime, u64);

/// The current `rift.toml` file state, or null when the file is absent or
/// unreadable — either way the next admission decides what that means.
fn configuration_fingerprint(root: &Path) -> Option<ConfigurationFingerprint> {
    let metadata = std::fs::metadata(root.join(WORKSPACE_CONFIGURATION_FILE)).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

#[tool_router(router = tool_router, vis = "pub(crate)")]
impl RiftMcp {
    /// Builds server from one direct-workspace snapshot, applying the
    /// admitted `rift.toml`'s `[source]` policy to the initial index. While
    /// `rift.toml` is invalid, the initial index still builds under the
    /// default policy; every request then fails as `configuration_invalid`
    /// until the file is fixed.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError`] when workspace cannot be indexed within bounds.
    ///
    /// # Cancel safety
    ///
    /// Dropping this future discards construction. An admitted blocking scan
    /// finishes in the bounded executor before releasing its capacity permit.
    pub async fn build(root: &Path, limits: WorkspaceIndexLimits) -> Result<Self, ReadError> {
        let root = root.to_path_buf();
        let build_root = root.clone();
        let published = bounded_blocking("initial index build", move || {
            let configuration = ConfigurationState::admit(&build_root);
            let visibility = configuration.source_visibility();
            let reads = ReadService::build(&build_root, limits, &visibility)?;
            Ok(Arc::new(PublishedWorkspace {
                reads: Arc::new(reads),
                configuration,
            }))
        })
        .await?;
        Ok(Self {
            root: root.clone(),
            limits,
            published: Arc::new(RwLock::new(published)),
            changes: Arc::new(ChangeService::new(&root)),
            change_lane: Arc::new(Mutex::new(())),
            tool_router: Self::tool_router(),
        })
    }

    /// Finds Rust declarations and their source by exact symbol name. Each hit
    /// carries the declaration and its source excerpt; `include_body: false` omits
    /// both. `rev` serves the lookup from a version-control revision instead of
    /// the current tree. Use `search` when the name is not exactly known.
    #[tool]
    async fn get_symbol(
        &self,
        Parameters(params): Parameters<GetSymbolParams>,
    ) -> Result<Json<GetSymbolResult>, ErrorData> {
        let rev = params.rev.clone();
        self.read_at(rev, move |reads| reads.get_symbol(&params))
            .await
    }

    /// Searches indexed Rust declarations and source lines by lexical `query`.
    /// `rev` searches a version-control revision instead of the current tree.
    /// Use `get_symbol` when the declaration name is known.
    #[tool]
    async fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<Json<SearchResult>, ErrorData> {
        let rev = params.rev.clone();
        self.read_at(rev, move |reads| reads.search(&params)).await
    }

    /// Lists the syntax nodes covering one UTF-8 byte position in one file,
    /// outermost first. Each identity carries a witness, so an address taken
    /// from this listing refuses cleanly once the file's bytes drift. `rev`
    /// lists the nodes as of a version-control revision instead of the
    /// current tree.
    #[tool]
    async fn nodes(
        &self,
        Parameters(params): Parameters<NodesParams>,
    ) -> Result<Json<NodesResult>, ErrorData> {
        let rev = params.rev.clone();
        self.read_at(rev, move |reads| reads.nodes(params)).await
    }

    /// Replaces one declaration addressed by symbol. The whole declaration
    /// includes its attached outer attributes and doc comments. The parser
    /// derives the span, so the caller supplies no offsets; a refusal
    /// names the failed precondition and leaves the workspace untouched.
    #[tool]
    async fn replace_symbol(
        &self,
        Parameters(params): Parameters<ReplaceSymbolParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.change(move |reads, changes| changes.replace_symbol(reads, &params))
            .await
    }

    /// Inserts a new declaration beside an anchor symbol, or content at a file
    /// target. Anchored insertions land beside the anchor's whole declaration,
    /// its attached outer attributes and doc comments included. A file target
    /// lands the body verbatim at the file's start or end, creating it first
    /// when `create_missing` is set and it is missing. A refusal names the
    /// failed precondition and leaves the workspace untouched.
    #[tool]
    async fn insert_symbol(
        &self,
        Parameters(params): Parameters<InsertSymbolParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.change(move |reads, changes| changes.insert_symbol(reads, &params))
            .await
    }

    /// Replaces one syntax node through a witnessed address from `nodes`.
    /// The server recomputes the witness before writing and refuses when the
    /// bytes drifted, so a stale address never splices into moved code.
    #[tool]
    async fn replace_node(
        &self,
        Parameters(params): Parameters<ReplaceNodeParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.change(move |reads, changes| changes.replace_node(reads, &params))
            .await
    }

    /// Applies unified-diff hunks to workspace files atomically. Hunk
    /// context guards the change; header line numbers are hints, as with
    /// `git apply`. A `/dev/null` header creates or deletes the file.
    #[tool]
    async fn patch(
        &self,
        Parameters(params): Parameters<PatchParams>,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.change(move |reads, changes| changes.patch(reads, &params))
            .await
    }

    /// Runs one read against the tree the request names — the current
    /// snapshot, or a snapshot built at the request's version-control
    /// revision — behind the admission gate every request passes.
    ///
    /// A revision snapshot is built per request from the workspace's git
    /// objects, under the same `[source]` policy and bounds as the current
    /// one; `[providers.history] enabled = false` refuses it.
    async fn read_at<Answer>(
        &self,
        rev: Option<rift_protocol::read::RevisionId>,
        operation: impl FnOnce(&ReadService) -> Result<Answer, ReadError> + Send + 'static,
    ) -> Result<Json<Answer>, ErrorData>
    where
        Answer: Send + 'static,
    {
        let published = self.published_workspace(wire::ErrorPhase::Read).await?;
        let configuration = published.configuration.admitted(wire::ErrorPhase::Read)?;
        let read_error = |error: ReadError| error.tool_error(wire::ErrorPhase::Read);
        let Some(rev) = rev else {
            let reads = Arc::clone(&published.reads);
            return bounded_blocking("current workspace read", move || operation(&reads))
                .await
                .map(Json)
                .map_err(read_error);
        };
        if !configuration.providers.history.enabled {
            return Err(read_error(ReadError::from(ReadFault::Unsupported {
                capability: "revision reads (providers.history disabled)",
            })));
        }
        let visibility = SourceVisibility::from(&configuration.source);
        let root = self.root.clone();
        let limits = self.limits;
        bounded_blocking("revision workspace read", move || {
            let reads = ReadService::at_revision(&root, &rev, limits, &visibility)?;
            operation(&reads)
        })
        .await
        .map(Json)
        .map_err(read_error)
    }

    /// Returns one atomically published index and configuration policy.
    async fn published_workspace(
        &self,
        phase: wire::ErrorPhase,
    ) -> Result<Arc<PublishedWorkspace>, ErrorData> {
        let root = self.root.clone();
        let fingerprint = bounded_blocking("configuration fingerprint", move || {
            Ok(configuration_fingerprint(&root))
        })
        .await
        .map_err(|error| error.tool_error(phase))?;
        let guard = self.published.read().await;
        let published = Arc::clone(&guard);
        drop(guard);
        if published.configuration.fingerprint == fingerprint {
            published.configuration.admitted(phase)?;
            return Ok(published);
        }
        self.refresh_configuration(phase).await
    }

    /// Rebuilds index and configuration together after `rift.toml` changes.
    async fn refresh_configuration(
        &self,
        phase: wire::ErrorPhase,
    ) -> Result<Arc<PublishedWorkspace>, ErrorData> {
        let root = self.root.clone();
        let limits = self.limits;
        let published = Arc::clone(&self.published);
        let change_lane = Arc::clone(&self.change_lane);
        let refreshed = bounded_blocking("configuration index rebuild", move || {
            let lane = change_lane
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let current = Arc::clone(&published.blocking_read());
            let fingerprint = configuration_fingerprint(&root);
            if current.configuration.fingerprint == fingerprint {
                drop(lane);
                return Ok(current);
            }
            let configuration = ConfigurationState::admit(&root);
            let visibility = configuration.source_visibility();
            let reads = Arc::new(ReadService::build(&root, limits, &visibility)?);
            let next = Arc::new(PublishedWorkspace {
                reads,
                configuration,
            });
            *published.blocking_write() = Arc::clone(&next);
            drop(lane);
            Ok(next)
        })
        .await
        .map_err(|error| error.tool_error(phase))?;
        refreshed.configuration.admitted(phase)?;
        Ok(refreshed)
    }

    /// Runs one change against the current snapshot and, when it lands,
    /// runs the workspace's hooks in the changed tree and swaps in a
    /// snapshot of the changed workspace.
    ///
    /// Hooks observe an already-applied change: their verdicts ride the
    /// result and never roll the change back. The snapshot is rebuilt after
    /// they ran, so reads also serve whatever a hook wrote into the tree,
    /// under the `[source]` policy this call already admitted. A rebuild
    /// failure after a landed change rides the result as a diagnostic
    /// rather than failing the call: the write happened, and the caller
    /// must not be told otherwise.
    ///
    /// Dropping this future after blocking work starts does not cancel that
    /// work. The serialized operation finishes through snapshot publication
    /// before releasing its lane.
    async fn change(
        &self,
        operation: impl FnOnce(&ReadService, &ChangeService) -> Result<ChangeResult, ReadError>
        + Send
        + 'static,
    ) -> Result<Json<ChangeResult>, ErrorData> {
        self.published_workspace(wire::ErrorPhase::Change).await?;
        let root = self.root.clone();
        let limits = self.limits;
        let published = Arc::clone(&self.published);
        let changes = Arc::clone(&self.changes);
        let change_lane = Arc::clone(&self.change_lane);
        bounded_blocking("workspace change", move || {
            let lane = change_lane
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let current = Arc::clone(&published.blocking_read());
            let configuration = match current.configuration.admitted(wire::ErrorPhase::Change) {
                Ok(configuration) => configuration,
                Err(error) => {
                    drop(lane);
                    return Ok(Err(error));
                }
            };
            let mut result = operation(&current.reads, &changes)?;
            if let ChangeResult::Applied { summary } = &mut result {
                Self::attach_hook_verdicts(&root, &configuration.hooks, summary);
                let visibility = SourceVisibility::from(&configuration.source);
                match ReadService::build(&root, limits, &visibility) {
                    Ok(rebuilt) => {
                        let next = Arc::new(PublishedWorkspace {
                            reads: Arc::new(rebuilt),
                            configuration: current.configuration.clone(),
                        });
                        *published.blocking_write() = next;
                    }
                    Err(error) => summary.diagnostics.push(stale_snapshot_diagnostic(&error)),
                }
            }
            drop(lane);
            Ok(Ok(Json(result)))
        })
        .await
        .map_err(|error| error.tool_error(wire::ErrorPhase::Change))?
    }

    /// Runs the configured hooks over one applied change and attaches what
    /// they established: a passing hook's configured guarantees become
    /// evidence, and every other outcome becomes an error finding.
    fn attach_hook_verdicts(root: &Path, hooks: &[CommandHook], summary: &mut ChangeSummary) {
        if hooks.is_empty() {
            return;
        }
        let runs = run_hooks(hooks, root, &summary.paths);
        for (hook, run) in hooks.iter().zip(&runs) {
            if run.status == HookStatus::Passed {
                summary
                    .guarantees
                    .extend(hook.guarantees.iter().map(|guarantee| GuaranteeEvidence {
                        kind: guarantee.kind,
                        scope: guarantee.scope.clone(),
                        hook: hook.id.clone(),
                        detail: guarantee.detail.clone(),
                    }));
            } else {
                summary.diagnostics.push(hook_failure_diagnostic(hook, run));
            }
        }
    }
}

/// Bytes of each captured hook stream a failure finding quotes. The finding
/// also states the full sizes, so a truncated quote stays distinguishable
/// from a short log.
const HOOK_FINDING_STREAM_BYTES_MAX: usize = 1_024;

/// The finding an applied change carries for one hook that did not pass:
/// what ended the run, then each non-empty stream's size and bounded quote.
fn hook_failure_diagnostic(hook: &CommandHook, run: &HookRun) -> rift_protocol::read::Diagnostic {
    let account = match &run.status {
        HookStatus::Passed => unreachable!(
            "a passing hook contributes guarantees, not findings: hook={:?}",
            hook.id
        ),
        HookStatus::Failed => match run.exit_code {
            Some(code) => format!("exited {code}"),
            None => "exited nonzero".to_owned(),
        },
        HookStatus::TimedOut => format!("killed after {}ms", hook.timeout_ms),
        HookStatus::Error(message) => message.clone(),
    };
    let mut message = format!("hook {} did not pass: {account}", hook.id);
    for (stream_name, stream) in [("stdout", &run.stdout), ("stderr", &run.stderr)] {
        if stream.total_bytes == 0 {
            continue;
        }
        let quoted = bounded_prefix(&stream.text, HOOK_FINDING_STREAM_BYTES_MAX);
        let _ = write!(
            message,
            "; {stream_name} ({} of {} bytes): {quoted}",
            quoted.len(),
            stream.total_bytes,
        );
    }
    rift_protocol::read::Diagnostic {
        severity: rift_protocol::read::Severity::Error,
        code: Some(DiagnosticCode::HookFailed.code()),
        message,
        span: None,
        related: Vec::new(),
        tags: Vec::new(),
        reliability: rift_protocol::read::DiagnosticReliability::Reliable,
        continuation: rift_protocol::read::DiagnosticContinuation::Unknown,
        extensions: rift_protocol::read::Extensions(std::collections::BTreeMap::new()),
        language: None,
    }
}

/// The longest prefix of `text` within `bytes_max` that ends on a character
/// boundary. The walk back is bounded by UTF-8 itself: at most three steps.
fn bounded_prefix(text: &str, bytes_max: usize) -> &str {
    if text.len() <= bytes_max {
        return text;
    }
    let mut end = bytes_max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The finding an applied change carries when the follow-up snapshot could
/// not be rebuilt: reads keep serving the pre-change snapshot until one can.
fn stale_snapshot_diagnostic(error: &ReadError) -> rift_protocol::read::Diagnostic {
    rift_protocol::read::Diagnostic {
        severity: rift_protocol::read::Severity::Warning,
        code: Some(DiagnosticCode::SnapshotStale.code()),
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
                 and replace_symbol, insert_symbol, replace_node, and patch change \
                 code atomically behind verified preconditions.",
            )
    }
}

/// Boundary view of a read failure: the projection a tool handler serves as
/// the JSON-RPC error object the design documents — code `-32000`, the
/// rendered failure line as `message`, and the typed [`wire::ErrorData`] as
/// `data`.
trait WireFailure {
    /// The JSON-RPC error object for this failure, naming the phase it
    /// stopped in.
    fn tool_error(&self, phase: wire::ErrorPhase) -> ErrorData;

    /// The typed wire payload for this failure.
    fn wire_error(&self, phase: wire::ErrorPhase) -> wire::ErrorData;

    /// The failure's source chain as bounded `causes` entries, outermost
    /// first. Each level inherits the outer classification, which the read
    /// error already resolved through the concrete failure it wraps.
    fn wire_causes(&self) -> Vec<wire::ErrorCause>;
}

impl<K: Fault> WireFailure for rift_core::Error<K> {
    fn tool_error(&self, phase: wire::ErrorPhase) -> ErrorData {
        let message = self.to_string();
        let data = serde_json::to_value(self.wire_error(phase)).ok();
        ErrorData::new(RIFT_ERROR_CODE, message, data)
    }

    fn wire_error(&self, phase: wire::ErrorPhase) -> wire::ErrorData {
        let descriptor = self.descriptor();
        wire::ErrorData {
            code: wire_code(descriptor.name()),
            message: self.to_string(),
            retry: descriptor.retry(),
            phase,
            diagnostics: Vec::new(),
            limit: None,
            causes: self.wire_causes(),
        }
    }

    fn wire_causes(&self) -> Vec<wire::ErrorCause> {
        let descriptor = self.descriptor();
        bounded_causes(
            wire_code(descriptor.name()),
            descriptor.retry(),
            std::error::Error::source(self),
        )
    }
}

/// Walks one source chain into bounded `causes` entries, outermost first.
/// Every level inherits the classification and retry guidance passed in,
/// which the failure already resolved through the concrete fault it wraps.
fn bounded_causes(
    code: wire::ErrorCode,
    retry: wire::RetryDirective,
    outermost: Option<&(dyn std::error::Error + 'static)>,
) -> Vec<wire::ErrorCause> {
    let mut causes = Vec::new();
    let mut source = outermost;
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

/// The wire code for one registry identity. The registry composes the wire
/// enum, so this is a projection, not a mapping; a CLI-only identity never
/// reaches this boundary, and classifies as `internal_error` if one does.
fn wire_code(name: ErrorName) -> wire::ErrorCode {
    match name {
        ErrorName::Wire(code) => code,
        ErrorName::Cli(_) => wire::ErrorCode::InternalError,
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;

    use rift_core::{CliCode, ErrorName, SourceVisibility};
    use rift_index::WorkspaceIndexLimits;
    use rift_protocol::error as wire;
    use rift_server::{ReadFault, ReadService};

    use super::WireFailure;
    use rmcp::ServiceError;
    use rmcp::ServiceExt as _;
    use rmcp::model::{CallToolRequestParams, ErrorCode};
    use serde_json::json;

    use super::RiftMcp;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    async fn fixture() -> TestResult<(tempfile::TempDir, RiftMcp)> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let server = RiftMcp::build(directory.path(), WorkspaceIndexLimits::default()).await?;
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
    async fn build_propagates_workspace_index_failure() {
        let error = RiftMcp::build(
            std::path::Path::new("not-a-real-rift-workspace"),
            WorkspaceIndexLimits::default(),
        )
        .await
        .expect_err("missing root must fail");
        assert!(matches!(error.fault(), ReadFault::Index(_)));
    }

    #[tokio::test]
    async fn client_lists_and_calls_exact_read_only_surface() -> TestResult {
        let (_directory, server) = fixture().await?;
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
                "patch",
                "replace_node",
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
        let (_directory, server) = fixture().await?;
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
        let (_directory, server) = fixture().await?;
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

    #[tokio::test]
    async fn concurrent_inserts_publish_from_serialized_fresh_snapshots() -> TestResult {
        let (directory, server) = fixture().await?;
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;
        let first_client = client.peer().clone();
        let second_client = client.peer().clone();
        let first = first_client.call_tool(
            CallToolRequestParams::new("insert_symbol").with_arguments(arguments(&json!({
                "anchor": "rift://symbol/rust/lib.rs/beacon",
                "position": "after",
                "body": "pub fn first_insert() {}"
            }))?),
        );
        let second = second_client.call_tool(
            CallToolRequestParams::new("insert_symbol").with_arguments(arguments(&json!({
                "anchor": "rift://symbol/rust/lib.rs/beacon",
                "position": "after",
                "body": "pub fn second_insert() {}"
            }))?),
        );
        let (first, second) = tokio::join!(first, second);
        for result in [first?, second?] {
            let structured = result
                .structured_content
                .ok_or("insert_symbol must return structured content")?;
            assert_eq!(structured["status"], json!("applied"));
        }
        let written = fs::read_to_string(directory.path().join("lib.rs"))?;
        assert!(
            written.contains("first_insert"),
            "first concurrent insert must survive: {written}"
        );
        assert!(
            written.contains("second_insert"),
            "second concurrent insert must survive: {written}"
        );

        client.cancel().await?;
        server_task.await?;
        Ok(())
    }

    #[tokio::test]
    async fn applied_change_reports_failed_snapshot_rebuild_as_warning() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() {}
",
        )?;
        let tight = rift_index::WorkspaceIndexLimits::new(4, 60, 60, 4, 100)
            .map_err(|error| error.to_string())?;
        let server = RiftMcp::build(directory.path(), tight).await?;
        let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let service = server
                .serve(server_transport)
                .await
                .expect("server must initialize");
            service.waiting().await.expect("server must stop cleanly");
        });
        let client = ().serve(client_transport).await?;

        let grown = "/// Grown far beyond the configured workspace byte bound.
pub fn beacon() -> u64 {
    7_000_000_000_000_000_000
}";
        let change = client
            .call_tool(
                CallToolRequestParams::new("replace_symbol").with_arguments(arguments(&json!({
                    "symbol": "rift://symbol/rust/lib.rs/beacon",
                    "body": grown
                }))?),
            )
            .await?;
        let structured = change
            .structured_content
            .ok_or("replace_symbol must return structured content")?;
        assert_eq!(structured["status"], json!("applied"));
        let findings = structured["summary"]["diagnostics"]
            .as_array()
            .ok_or("summary must carry diagnostics")?;
        assert!(
            findings.iter().any(|finding| {
                finding["severity"] == json!("warning")
                    && finding["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("could not refresh"))
            }),
            "a failed rebuild after a landed change must ride the result as a \
             warning: {structured:#}"
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
        let (_directory, server) = fixture().await?;
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

    #[test]
    fn cli_identity_projects_to_internal_error_on_the_wire() {
        assert_eq!(
            super::wire_code(ErrorName::Cli(CliCode::ArtifactStale)),
            wire::ErrorCode::InternalError
        );
    }

    #[derive(Debug)]
    struct Link {
        depth: usize,
        inner: Option<Box<Link>>,
    }

    impl std::fmt::Display for Link {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "link {}", self.depth)
        }
    }

    impl Error for Link {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.inner
                .as_deref()
                .map(|link| link as &(dyn Error + 'static))
        }
    }

    #[test]
    fn cause_walk_stops_at_the_declared_bound() {
        let mut chained = Link {
            depth: 0,
            inner: None,
        };
        for depth in 1..=super::ERROR_CAUSES_MAX + 2 {
            chained = Link {
                depth,
                inner: Some(Box::new(chained)),
            };
        }
        let causes = super::bounded_causes(
            wire::ErrorCode::StorageFailure,
            wire::RetryDirective::Never,
            Some(&chained),
        );
        assert_eq!(
            causes.len(),
            super::ERROR_CAUSES_MAX,
            "a chain deeper than the bound must truncate at the bound"
        );
    }

    fn probe_hook() -> rift_protocol::configuration::CommandHook {
        use rift_protocol::configuration::{ChangedPaths, Determinism, HookKind, HookType};
        rift_protocol::configuration::CommandHook {
            r#type: HookType::Command,
            id: "tests".to_owned(),
            kind: HookKind::Test,
            program: "cargo".to_owned(),
            arguments: vec!["test".to_owned()],
            changed_paths: ChangedPaths::None,
            working_directory: rift_protocol::read::ProjectPath(String::new()),
            environment: std::collections::BTreeMap::new(),
            timeout_ms: 120_000,
            output_limit_bytes: 4_096,
            guarantees: Vec::new(),
            determinism: Determinism::Deterministic,
        }
    }

    fn silent_run(status: rift_server::HookStatus, exit_code: Option<i32>) -> rift_server::HookRun {
        rift_server::HookRun {
            id: "tests".to_owned(),
            status,
            exit_code,
            stdout: rift_server::CapturedStream::default(),
            stderr: rift_server::CapturedStream::default(),
        }
    }

    #[test]
    fn failed_hook_finding_quotes_exit_code_and_nonempty_streams() {
        use rift_server::{CapturedStream, HookStatus};
        let mut run = silent_run(HookStatus::Failed, Some(1));
        run.stdout = CapturedStream {
            text: "boom".to_owned(),
            captured_bytes: 4,
            total_bytes: 4,
            truncated: false,
        };
        let finding = super::hook_failure_diagnostic(&probe_hook(), &run);
        assert_eq!(finding.severity, rift_protocol::read::Severity::Error);
        assert_eq!(finding.code.as_deref(), Some("rift.hook.failed"));
        assert!(
            finding.message.contains("exited 1")
                && finding.message.contains("stdout (4 of 4 bytes): boom")
                && !finding.message.contains("stderr"),
            "{}",
            finding.message
        );
    }

    #[test]
    #[should_panic(expected = "a passing hook contributes guarantees, not findings")]
    fn passing_hook_finding_is_a_programmer_error() {
        let run = silent_run(rift_server::HookStatus::Passed, Some(0));
        let _ = super::hook_failure_diagnostic(&probe_hook(), &run);
    }

    #[test]
    fn hook_finding_accounts_for_every_non_passing_outcome() {
        use rift_server::HookStatus;
        let cases = [
            (HookStatus::Failed, None, "exited nonzero"),
            (HookStatus::TimedOut, None, "killed after 120000ms"),
            (
                HookStatus::Error("failed to launch: missing".to_owned()),
                None,
                "failed to launch: missing",
            ),
        ];
        for (status, exit_code, expected) in cases {
            let finding =
                super::hook_failure_diagnostic(&probe_hook(), &silent_run(status, exit_code));
            assert!(
                finding.message.contains(expected),
                "{expected} missing from {}",
                finding.message
            );
        }
    }

    #[test]
    fn bounded_prefix_cuts_on_character_boundaries() {
        assert_eq!(super::bounded_prefix("short", 16), "short");
        assert_eq!(super::bounded_prefix("ééé", 3), "é");
        assert_eq!(super::bounded_prefix("ééé", 4), "éé");
    }

    #[test]
    fn stale_snapshot_finding_carries_its_code_and_the_render() {
        let error = rift_server::ReadError::from(ReadFault::Unsupported {
            capability: "probe",
        });
        let finding = super::stale_snapshot_diagnostic(&error);
        assert_eq!(finding.code.as_deref(), Some("rift.snapshot.stale"));
        assert_eq!(finding.severity, rift_protocol::read::Severity::Warning);
        assert!(
            finding.message.contains("the change landed"),
            "{}",
            finding.message
        );
    }

    #[test]
    fn wire_causes_walk_the_source_chain_with_inherited_classification() {
        let error = ReadService::build(
            std::path::Path::new("not-a-real-rift-workspace"),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
        )
        .expect_err("missing root must fail");
        let causes = error.wire_causes();
        assert!(!causes.is_empty(), "sourced failure must yield causes");
        assert!(causes.len() <= super::ERROR_CAUSES_MAX);
        let code = super::wire_code(error.descriptor().name());
        for cause in &causes {
            assert!(!cause.message.is_empty(), "cause message must be rendered");
            assert_eq!(cause.code, code);
        }
    }
}
