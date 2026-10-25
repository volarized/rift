mod query;
mod source;
mod symbol;
mod traversal;
pub(crate) use query::{QueryGlobalSelection, SavedQuery};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use symbol::SavedRead;
pub(crate) use symbol::SavedSymbol;

use rift_error::{RiftError, errors};
use rift_protocol::read::{ReadWarning, SearchParams};
use rift_protocol::symbol_read::CapturedView;
use rift_server::SearchCandidateInputs;
use rmcp::handler::server::tool::IntoCallToolResult as _;
use rmcp::model::{CallToolRequestParams, CallToolResponse};

use crate::failure::{McpErrorExt as _, WireFailure as _};
use serde::{Deserialize, Serialize};

use super::{ReadAnswer, RiftMcp, ToolFailure, wire};

const BYTES_MAX: usize = crate::global::candidates::CANDIDATE_BYTES_MAX;
const SLOTS_MAX: usize = 256;

pub(in crate::server) fn write_captured_context(
    result: &mut Result<CallToolResponse, rmcp::ErrorData>,
    captured: Option<&Mutex<Option<crate::global::candidates::SavedCandidateBackend>>>,
) {
    if let Ok(CallToolResponse::Complete(response)) = result
        && let Some(captured) = captured
        && let Some(selected) = captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    {
        let meta = response
            .meta
            .get_or_insert_with(rmcp::model::MetaObject::new);
        meta.insert("backend".to_owned(), serde_json::json!(selected.backend));
        meta.insert("root".to_owned(), serde_json::json!(selected.root));
        meta.insert("owner".to_owned(), serde_json::json!(selected.owner));
        meta.insert("view".to_owned(), serde_json::json!(selected.view));
    }
}

fn global_context(
    context: Option<&Mutex<Option<crate::global::candidates::SavedCandidateBackend>>>,
    endpoint: &str,
    view: Option<&rift_protocol::symbol_read::CapturedViewId>,
    operation: &str,
) -> Result<(), ToolFailure> {
    let Some(context) = context else {
        return Ok(());
    };
    let context = context
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(selected) = context.as_ref() else {
        return Ok(());
    };
    if selected.backend != "global"
        || selected.root != endpoint
        || selected.owner.is_some()
        || view != Some(&selected.view.id)
    {
        return Err(captured_read_refusal(
            operation,
            &selected.view.id,
            rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
        ));
    }
    if crate::proxy::captured_expiry(&selected.view).map_err(read_failure)? <= SystemTime::now() {
        return Err(captured_read_refusal(
            operation,
            &selected.view.id,
            rift_protocol::symbol_read::SymbolUnavailableReason::ViewExpired,
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchCandidateRequest {
    pub(crate) params: SearchParams,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) view: Option<CapturedView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchCandidateResponse {
    pub(crate) root: String,
    pub(crate) owner: rift_protocol::identity::SymbolOwner,
    pub(crate) view: CapturedView,
    pub(crate) inputs: SearchCandidateInputs,
    pub(crate) ranking: rift_protocol::configuration::RankingConfiguration,
    pub(crate) results_max: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) global: Option<crate::global::candidates::SearchCandidatePool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum SearchCandidateResult {
    Captured(Box<SearchCandidateResponse>),
    Unavailable(rift_protocol::search_read::SearchUnavailable),
}

#[derive(Debug)]
pub(super) struct CandidateResponses {
    bytes: Mutex<Vec<SavedCandidateBytes>>,
}

#[derive(Debug)]
struct SavedCandidateBytes {
    id: String,
    expires: SystemTime,
    bytes: Vec<u8>,
}

impl Default for CandidateResponses {
    fn default() -> Self {
        Self {
            bytes: Mutex::new(Vec::with_capacity(SLOTS_MAX)),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SavedResponse {
    params: SearchParams,
    response: SearchCandidateResponse,
}

impl ReadAnswer for SearchCandidateInputs {
    fn warnings_mut(&mut self) -> &mut Vec<ReadWarning> {
        &mut self.warnings
    }
}

fn validate_candidate_query(params: &SearchParams) -> Result<(), ToolFailure> {
    let Some(query) = params.query.as_deref() else {
        return Ok(());
    };
    rift_ranking::ParsedQuery::parse(query).map_err(|error| {
        read_failure(
            errors::server::read_invalid()
                .field("query")
                .violation(error.detail())
                .error(),
        )
    })?;
    Ok(())
}

fn preparing_candidates(
    resolved: &super::ResolvedWorkspace,
    warnings: &[ReadWarning],
) -> Option<rift_protocol::search_read::SearchUnavailable> {
    let warning = resolved
        .published
        .preparation
        .as_ref()
        .and_then(crate::validation::LocalIndexPreparation::warning)?;
    let mut answer_warnings = Vec::new();
    answer_warnings.extend(warnings.iter().cloned());
    answer_warnings.extend(resolved.stale.clone());
    answer_warnings.push(warning);
    Some(rift_protocol::search_read::SearchUnavailable::Unavailable {
        reason: rift_protocol::symbol_read::SymbolUnavailableReason::IndexPreparing,
        view: None,
        warnings: answer_warnings,
    })
}

impl RiftMcp {
    pub(in crate::server) async fn captured_query(
        &self,
        params: &SearchParams,
    ) -> Result<Option<rift_protocol::search_read::SearchReadResult>, ToolFailure> {
        use rift_protocol::symbol_read::SymbolUnavailableReason;
        let unavailable = |reason, view| {
            rift_protocol::search_read::SearchReadResult::Unavailable(
                rift_protocol::search_read::SearchUnavailable::Unavailable {
                    reason,
                    view,
                    warnings: Vec::new(),
                },
            )
        };
        if params.traversal.is_some() {
            return self.captured_traversal(params, None).await.map(Some);
        }
        if params.pattern.is_some()
            || params.change.is_some()
            || params.rev.is_some()
            || params.target == rift_protocol::read::SearchParamsTarget::Commit
        {
            return Ok(params
                .view
                .as_ref()
                .map(|_| unavailable(SymbolUnavailableReason::ViewContext, None)));
        }
        let normalized = crate::proxy::normalized_params(params);
        let selected = match params.view.as_ref() {
            Some(view) => {
                let rows = self
                    .candidate_responses
                    .bytes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(row) = rows.iter().find(|row| row.id == view.as_str()) else {
                    return Ok(Some(unavailable(
                        SymbolUnavailableReason::ViewContext,
                        None,
                    )));
                };
                let saved: SavedRead =
                    serde_json::from_slice(&row.bytes).map_err(transport_failure)?;
                let SavedRead::Search(saved) = saved else {
                    return Ok(Some(unavailable(
                        SymbolUnavailableReason::ViewContext,
                        None,
                    )));
                };
                if row.expires <= SystemTime::now() {
                    return Ok(Some(unavailable(
                        SymbolUnavailableReason::ViewExpired,
                        Some(saved.response.view),
                    )));
                }
                if crate::proxy::normalized_params(&saved.params) != normalized {
                    return Ok(Some(unavailable(
                        SymbolUnavailableReason::ViewContext,
                        Some(saved.response.view),
                    )));
                }
                Some(saved.response.view)
            }
            None => None,
        };
        let arguments = serde_json::to_value(SearchCandidateRequest {
            params: normalized,
            view: selected,
        })
        .map_err(transport_failure)?
        .as_object()
        .cloned()
        .ok_or_else(|| super::candidates::unavailable("candidate request is not an object"))?;
        let captured = self
            .search_candidates_request(
                CallToolRequestParams::new("search_candidates").with_arguments(arguments),
            )
            .await?;
        let captured = match captured {
            SearchCandidateResult::Captured(answer) => *answer,
            SearchCandidateResult::Unavailable(answer) => {
                return Ok(Some(
                    rift_protocol::search_read::SearchReadResult::Unavailable(answer),
                ));
            }
        };
        let mut pools = vec![crate::global::candidates::SearchCandidatePool {
            backend: "local".to_owned(),
            root: captured.root,
            owner: Some(captured.owner),
            view: captured.view.clone(),
            inputs: captured.inputs,
        }];
        pools.extend(captured.global);
        Self::merge_captured_query(
            params,
            &captured.ranking,
            captured.view,
            captured.results_max,
            &pools,
        )
        .map(Some)
    }

    fn merge_captured_query(
        params: &SearchParams,
        ranking: &rift_protocol::configuration::RankingConfiguration,
        view: CapturedView,
        results_max: usize,
        pools: &[crate::global::candidates::SearchCandidatePool],
    ) -> Result<rift_protocol::search_read::SearchReadResult, ToolFailure> {
        let weights = rift_ranking::RankingWeights::new(
            ranking.identifier_weight,
            ranking.lexical_weight,
            ranking.vector_weight,
            ranking.fusion_k,
        )
        .map_err(read_failure)?;
        let mut result =
            crate::global::candidates::merge_candidate_pools(params, pools, weights, results_max)
                .map_err(read_failure)?;
        result.view = Some(view);
        Ok(rift_protocol::search_read::SearchReadResult::Results(
            Box::new(result),
        ))
    }

    async fn current_captured_read<Answer>(
        &self,
        resolved: &super::ResolvedWorkspace,
        include_local_preparation: bool,
        operation: impl FnOnce(
            &rift_server::ReadService,
            Vec<ReadWarning>,
        ) -> Result<Answer, rift_error::RiftError>
        + Send
        + 'static,
    ) -> Result<crate::output::Json<Answer>, ToolFailure>
    where
        Answer: Send + 'static,
    {
        resolved
            .published
            .configuration
            .accepted_for_tool(wire::ErrorPhase::Read)?;
        let reads = Arc::clone(&resolved.published.reads);
        if let Some(history) = &self.history {
            reads.attach_history_store(history.stored());
        }
        let mut warnings = Vec::new();
        warnings.extend(resolved.stale.clone());
        if include_local_preparation {
            warnings.extend(
                resolved
                    .published
                    .preparation
                    .as_ref()
                    .and_then(crate::validation::LocalIndexPreparation::warning),
            );
        }
        self.blocking
            .run("current workspace read", move || {
                operation(&reads, warnings)
            })
            .await
            .map(crate::output::Json)
            .map_err(|error| error.mcp().tool_failure(wire::ErrorPhase::Read))
    }

    pub(super) async fn search_candidates_call(
        &self,
        request: CallToolRequestParams,
        operation: &rift_tracing::Span,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let result = self.search_candidates_request(request).await;
        match result {
            Ok(answer) => {
                let value = serde_json::to_value(answer).map_err(|source| {
                    let error = errors::mcp::forward_failed().source(source).error();
                    operation.record("error.type", error.slug().as_str());
                    error.mcp().tool_error(wire::ErrorPhase::Read)
                })?;
                Ok(rmcp::model::CallToolResult::structured(value).into())
            }
            Err(failure) => {
                if let ToolFailure::Registered(error, _) = &failure {
                    operation.record("error.type", error.registered().slug().as_str());
                }
                failure.into_call_tool_result()
            }
        }
    }

    async fn search_candidates_request(
        &self,
        request: CallToolRequestParams,
    ) -> Result<SearchCandidateResult, ToolFailure> {
        let arguments = request.arguments.unwrap_or_default();
        crate::proxy::encode_bounded(&arguments, BYTES_MAX).map_err(transport_failure)?;
        let requested: SearchCandidateRequest =
            serde_json::from_value(arguments.into()).map_err(|error| {
                errors::mcp::parameter_invalid()
                    .tool("search_candidates")
                    .field("arguments")
                    .accepted(error.to_string())
                    .mcp()
                    .tool_failure(wire::ErrorPhase::Read)
            })?;
        let views = self
            .local_views
            .get()
            .ok_or_else(|| unavailable("retained read material is unavailable"))?;
        if let Some(view) = &requested.view {
            let response = self.saved_candidates(&requested.params, view)?;
            let accepted = views
                .captured_view(
                    view.id.as_str(),
                    std::path::Path::new(&response.root),
                    &response.owner,
                )
                .map_err(read_failure)?
                .map_err(|reason| unavailable(&format!("{reason:?}")))?;
            if &accepted != view {
                return Err(unavailable("captured view context does not match"));
            }
            return Ok(SearchCandidateResult::Captured(Box::new(response)));
        }
        validate_candidate_query(&requested.params)?;
        let (resolved, answer, warnings, references, deadline) =
            self.current_search_inputs(&requested.params).await?;
        if let Some(answer) = preparing_candidates(&resolved, &warnings) {
            return Ok(SearchCandidateResult::Unavailable(answer));
        }
        let ready = resolved.published.preparation.is_none();
        let accepted = resolved
            .published
            .configuration
            .accepted_for_tool(wire::ErrorPhase::Read)?;
        let params = requested.params.clone();
        let (mut inputs, material) = self
            .current_captured_read(&resolved, true, move |reads, capture_warnings| {
                let (mut inputs, material) =
                    reads.search_candidate_view_input(&params, &answer, &references, ready)?;
                inputs.warnings.extend(capture_warnings.iter().cloned());
                let material = material.with_warnings(capture_warnings)?;
                let selection = material.provider_selection(&accepted)?;
                let material = material.with_provider_selection(selection)?;
                Ok((inputs, material))
            })
            .await?
            .0;
        inputs.warnings.extend(warnings);
        if requested.params.scope == rift_protocol::read::SearchScope::Global {
            inputs.precise.clear();
            inputs.broad.clear();
        }
        let (global, global_warnings) = self
            .package_candidate_pool(&resolved, &requested.params, deadline)
            .await?;
        inputs.warnings.extend(global_warnings);
        let configuration = resolved.published.configuration.search_configuration();
        let results_max = usize::try_from(configuration.results)
            .map_err(|_| unavailable("configured result bound does not fit this server"))?;
        let root = material
            .root()
            .to_str()
            .ok_or_else(|| unavailable("project root is not UTF-8"))?
            .to_owned();
        let owner = material.owner().clone();
        let mut response = None;
        views
            .capture_with(material, |view| {
                let answer = SearchCandidateResponse {
                    root,
                    owner,
                    view: view.clone(),
                    inputs,
                    ranking: configuration.ranking,
                    results_max,
                    global,
                };
                self.save_candidates(requested.params, answer.clone())?;
                response = Some(answer);
                Ok(true)
            })
            .map_err(read_failure)?;
        response
            .map(|response| SearchCandidateResult::Captured(Box::new(response)))
            .ok_or_else(|| unavailable("retained read material exceeds the byte or view bound"))
    }

    async fn package_candidate_pool(
        &self,
        resolved: &super::ResolvedWorkspace,
        params: &SearchParams,
        deadline: super::RequestDeadline,
    ) -> Result<
        (
            Option<crate::global::candidates::SearchCandidatePool>,
            Vec<rift_protocol::read::ReadWarning>,
        ),
        ToolFailure,
    > {
        if params.target == rift_protocol::read::SearchParamsTarget::File
            || matches!(
                params.scope,
                rift_protocol::read::SearchScope::Local
                    | rift_protocol::read::SearchScope::NamedLocal { .. }
                    | rift_protocol::read::SearchScope::ProfileLocal { .. }
            )
        {
            return Ok((None, Vec::new()));
        }
        let query = params
            .query
            .as_deref()
            .ok_or_else(|| unavailable("search query is required"))?;
        let query = rift_ranking::ParsedQuery::parse(query).map_err(read_failure)?;
        let context = crate::global::ReadContext::accepted(
            &resolved.published.reads,
            &params.scope,
            params.rev.as_ref(),
            &params.packages,
        )
        .map_err(read_failure)?;
        let configuration = resolved.published.configuration.global_configuration();
        let (route, mut captured) = self
            .global_read(
                deadline,
                &configuration,
                &context,
                |client, packages| async move {
                    crate::global::captured_package_search(&client, params, &query, &packages).await
                },
            )
            .await;
        captured.candidates.warnings.extend(route.warnings());
        let warnings = std::mem::take(&mut captured.candidates.warnings);
        let pool = crate::global::candidates::package_candidate_pool(
            params,
            configuration.endpoint,
            captured,
        )
        .map_err(read_failure)?;
        Ok((pool, warnings))
    }

    fn save_candidates(
        &self,
        params: SearchParams,
        response: SearchCandidateResponse,
    ) -> Result<(), RiftError> {
        let id = response.view.id.as_str().to_owned();
        let expires = crate::proxy::captured_expiry(&response.view)?;
        let encoded = crate::proxy::encode_bounded(
            &SavedRead::Search(Box::new(SavedResponse { params, response })),
            BYTES_MAX,
        )
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        self.candidate_responses.save_bytes(id, expires, encoded)
    }

    pub(super) fn captured_nodes(
        &self,
        params: &rift_protocol::read::NodesParams,
        view: &rift_protocol::symbol_read::CapturedViewId,
    ) -> Result<rift_protocol::read::NodesResult, ToolFailure> {
        let context = self.captured_context(view, "nodes")?;
        let owner = context.owner;
        let root = std::path::Path::new(&context.root);
        let views = self.local_views.get().ok_or_else(|| {
            captured_node_refusal(
                view,
                rift_protocol::symbol_read::SymbolUnavailableReason::ViewExpired,
            )
        })?;
        let (nodes, selected) = views
            .nodes(params, view, root, &owner)
            .map_err(read_failure)?
            .map_err(|reason| captured_node_refusal(view, reason))?;
        if nodes.view.as_ref() != Some(&selected) || selected.id != *view {
            return Err(captured_node_refusal(
                view,
                rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
            ));
        }
        Ok(nodes)
    }

    fn saved_candidates(
        &self,
        params: &SearchParams,
        view: &CapturedView,
    ) -> Result<SearchCandidateResponse, ToolFailure> {
        let saved = self
            .candidate_responses
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bytes = saved
            .iter()
            .find(|row| row.id == view.id.as_str())
            .ok_or_else(|| unavailable("captured candidates are unavailable"))?;
        let SavedRead::Search(saved) =
            serde_json::from_slice(&bytes.bytes).map_err(transport_failure)?
        else {
            return Err(unavailable(
                "captured candidate request context does not match",
            ));
        };
        if crate::proxy::normalized_params(&saved.params) != crate::proxy::normalized_params(params)
            || &saved.response.view != view
        {
            return Err(unavailable(
                "captured candidate request context does not match",
            ));
        }
        Ok(saved.response)
    }
}

fn read_failure(error: RiftError) -> ToolFailure {
    error.mcp().tool_failure(wire::ErrorPhase::Read)
}

fn transport_failure(source: serde_json::Error) -> ToolFailure {
    read_failure(errors::mcp::forward_failed().source(source).error())
}

fn unavailable(detail: &str) -> ToolFailure {
    read_failure(
        errors::server::read_unavailable()
            .operation("search_candidates")
            .detail(detail)
            .error(),
    )
}

fn captured_read_refusal(
    operation: &str,
    view: &rift_protocol::symbol_read::CapturedViewId,
    reason: rift_protocol::symbol_read::SymbolUnavailableReason,
) -> ToolFailure {
    use rift_protocol::symbol_read::SymbolUnavailableReason;
    let error = match reason {
        SymbolUnavailableReason::ViewExpired => errors::server::search_view_expired()
            .operation(operation)
            .view(view.as_str())
            .error(),
        SymbolUnavailableReason::ViewContext => errors::server::search_view_context()
            .operation(operation)
            .view(view.as_str())
            .error(),
        _ => errors::server::search_coverage_unavailable()
            .operation(operation)
            .view(view.as_str())
            .error(),
    };
    read_failure(error)
}

fn captured_node_refusal(
    view: &rift_protocol::symbol_read::CapturedViewId,
    reason: rift_protocol::symbol_read::SymbolUnavailableReason,
) -> ToolFailure {
    captured_read_refusal("nodes", view, reason)
}

#[cfg(test)]
mod captured_node_error_tests {
    use super::captured_node_refusal;
    use super::wire;
    use crate::{failure::WireFailure, output::ToolFailure};
    use rift_protocol::symbol_read::{CapturedViewId, SymbolUnavailableReason};

    #[test]
    fn nodes_view_refusals_keep_registered_code_and_safe_guidance() {
        let view = CapturedViewId::parse("claims.nodes").expect("issued fixture key");
        for (reason, slug, guidance) in [
            (
                SymbolUnavailableReason::ViewExpired,
                "rift.server.search_view_expired",
                "capture a new view",
            ),
            (
                SymbolUnavailableReason::ViewContext,
                "rift.server.search_view_context",
                "matching workspace",
            ),
        ] {
            let ToolFailure::Registered(error, phase) = captured_node_refusal(&view, reason) else {
                panic!("captured refusal must retain its registered error");
            };
            assert_eq!(error.registered().slug().as_str(), slug);
            let wire = error.wire_error(phase);
            assert_eq!(wire.code, wire::ErrorCode::InvalidRequest);
            assert_eq!(wire.retry, wire::RetryDirective::Never);
            assert!(wire.message.contains(view.as_str()));
            assert!(wire.message.contains(guidance));
            assert!(
                wire.causes.is_empty(),
                "expected refusal has no invented source"
            );
        }
    }
}

#[cfg(test)]
mod captured_search_startup_tests {
    use super::{RiftMcp, SearchCandidateRequest, SearchCandidateResult, SearchParams};
    use rmcp::model::CallToolRequestParams;

    #[tokio::test]
    async fn direct_search_keeps_original_capture_after_source_changes()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let directory = tempfile::tempdir()?;
        super::super::hermetic_workspace(directory.path(), "")?;
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='captured_fixture'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
        )?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, super::super::WorkspaceIndexLimits::default()).await?;
        let mut params: SearchParams = serde_json::from_value(serde_json::json!({
            "query": "beacon", "scope": "local", "include": ["source"]
        }))?;
        let initial = server
            .discovery_search(
                super::super::Parameters(params.clone()),
                rmcp::model::Extensions::default(),
            )
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0;
        let rift_protocol::search_read::SearchReadResult::Results(initial) = initial else {
            return Err("initial search must return captured results".into());
        };
        assert!(!initial.results.is_empty());
        let view = initial
            .view
            .clone()
            .ok_or("initial search must issue a view")?;
        assert!(
            initial
                .results
                .iter()
                .all(|hit| hit.view.as_ref() == Some(&view))
        );
        std::fs::write(directory.path().join("lib.rs"), "pub fn replacement() {}\n")?;
        params.view = Some(view.id.clone());
        let later = server
            .discovery_search(
                super::super::Parameters(params.clone()),
                rmcp::model::Extensions::default(),
            )
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0;
        let rift_protocol::search_read::SearchReadResult::Results(later) = later else {
            return Err("held search must return original results".into());
        };
        assert_eq!(later, initial);
        params.page_index = 1;
        let page = server
            .discovery_search(
                super::super::Parameters(params.clone()),
                rmcp::model::Extensions::default(),
            )
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0;
        let rift_protocol::search_read::SearchReadResult::Results(page) = page else {
            return Err("empty later page must keep capture".into());
        };
        assert!(page.results.is_empty());
        assert_eq!(page.view.as_ref(), Some(&view));
        params.query = Some("replacement".to_owned());
        assert!(matches!(
            server
                .discovery_search(
                    super::super::Parameters(params),
                    rmcp::model::Extensions::default()
                )
                .await
                .map_err(|failure| format!("{failure:?}"))?
                .0,
            rift_protocol::search_read::SearchReadResult::Unavailable(
                rift_protocol::search_read::SearchUnavailable::Unavailable {
                    reason: rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
                    ..
                }
            )
        ));
        Ok(())
    }

    #[tokio::test]
    async fn captured_search_empty_inputs_keep_original_view_and_request_context()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let directory = tempfile::tempdir()?;
        super::super::hermetic_workspace(directory.path(), "")?;
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='captured_fixture'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
        )?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let root = directory.path().canonicalize()?;
        let server =
            RiftMcp::build_settled(&root, super::super::WorkspaceIndexLimits::default()).await?;
        let params: SearchParams = serde_json::from_value(serde_json::json!({
            "query": "captured_nonexistent_symbol_329b", "scope": "local"
        }))?;
        let request = serde_json::to_value(SearchCandidateRequest {
            params: params.clone(),
            view: None,
        })?
        .as_object()
        .ok_or("candidate arguments must be an object")?
        .clone();
        let captured = server
            .search_candidates_request(
                CallToolRequestParams::new("search_candidates").with_arguments(request),
            )
            .await
            .map_err(|failure| format!("{failure:?}"))?;
        let SearchCandidateResult::Captured(captured) = captured else {
            return Err("settled candidate read must capture results".into());
        };
        assert!(
            captured
                .inputs
                .precise
                .iter()
                .all(|input| input.order.is_empty())
        );
        assert!(
            captured
                .inputs
                .broad
                .iter()
                .all(|input| input.order.is_empty())
        );
        assert_eq!(std::path::Path::new(&captured.root), server.root);
        let authority = server
            .local_views
            .get()
            .ok_or("startup must initialize local views")?;
        let selected = authority
            .captured_view(captured.view.id.as_str(), &server.root, &captured.owner)?
            .map_err(|reason| format!("{reason:?}"))?;
        assert_eq!(selected, captured.view);
        let continued = server
            .saved_candidates(&params, &selected)
            .map_err(|failure| format!("{failure:?}"))?;
        assert_eq!(continued.view, selected);
        let mut changed = params;
        changed.query = Some("beacon".to_owned());
        assert!(server.saved_candidates(&changed, &selected).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn captured_search_continuation_keeps_startup_root_after_symlink_changes()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let directory = tempfile::tempdir()?;
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let link = directory.path().join("project");
        for root in [&first, &second] {
            std::fs::create_dir(root)?;
            super::super::hermetic_workspace(root, "")?;
            std::fs::write(
                root.join("Cargo.toml"),
                "[package]\nname='captured_fixture'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
            )?;
        }
        std::fs::write(first.join("lib.rs"), "pub fn beacon() {}\n")?;
        std::fs::write(second.join("lib.rs"), "pub fn replacement() {}\n")?;
        std::os::unix::fs::symlink(&first, &link)?;
        let server =
            RiftMcp::build_settled(&link, super::super::WorkspaceIndexLimits::default()).await?;
        assert_eq!(server.root, link, "engine root retains caller spelling");
        let params: SearchParams =
            serde_json::from_value(serde_json::json!({"query":"beacon","scope":"local"}))?;
        let arguments = serde_json::to_value(SearchCandidateRequest {
            params: params.clone(),
            view: None,
        })?
        .as_object()
        .ok_or("candidate arguments must be an object")?
        .clone();
        let captured = server
            .search_candidates_request(
                CallToolRequestParams::new("search_candidates").with_arguments(arguments),
            )
            .await
            .map_err(|failure| format!("{failure:?}"))?;
        let SearchCandidateResult::Captured(captured) = captured else {
            return Err("settled candidate read must capture results".into());
        };
        let original_root = first.canonicalize()?;
        assert_eq!(std::path::Path::new(&captured.root), original_root);
        std::fs::remove_file(&link)?;
        std::os::unix::fs::symlink(&second, &link)?;
        assert_ne!(link.canonicalize()?, original_root);
        let arguments = serde_json::to_value(SearchCandidateRequest {
            params,
            view: Some(captured.view.clone()),
        })?
        .as_object()
        .ok_or("candidate arguments must be an object")?
        .clone();
        let continued = server
            .search_candidates_request(
                CallToolRequestParams::new("search_candidates").with_arguments(arguments),
            )
            .await
            .map_err(|failure| format!("{failure:?}"))?;
        let SearchCandidateResult::Captured(continued) = continued else {
            return Err("settled candidate read must capture results".into());
        };
        assert_eq!(continued.view, captured.view);
        assert_eq!(continued.root, captured.root);
        assert_eq!(continued.owner, captured.owner);
        let authority = server.local_views.get().ok_or("captured authority")?;
        let foreign = authority.captured_view(
            captured.view.id.as_str(),
            &second.canonicalize()?,
            &captured.owner,
        )?;
        assert_eq!(
            foreign,
            Err(rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext)
        );
        Ok(())
    }
}

impl CandidateResponses {
    fn save_bytes(
        &self,
        id: String,
        expires: SystemTime,
        encoded: Vec<u8>,
    ) -> Result<(), RiftError> {
        let mut saved = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        saved.retain(|row| row.expires > SystemTime::now());
        let retained = saved.iter().fold(
            saved
                .capacity()
                .checked_mul(std::mem::size_of::<SavedCandidateBytes>())
                .and_then(|bytes| bytes.checked_add(id.capacity()))
                .and_then(|bytes| bytes.checked_add(encoded.capacity())),
            |sum, row| {
                sum.and_then(|bytes| bytes.checked_add(row.id.capacity()))
                    .and_then(|bytes| bytes.checked_add(row.bytes.capacity()))
            },
        );
        if saved.len() >= SLOTS_MAX
            || retained.is_none_or(|bytes| bytes > BYTES_MAX)
            || saved.iter().any(|row| row.id == id)
        {
            let stored_bytes = saved
                .iter()
                .try_fold(0usize, |sum, row| sum.checked_add(row.bytes.len()));
            let stored_capacity = saved
                .iter()
                .try_fold(0usize, |sum, row| sum.checked_add(row.bytes.capacity()));
            rift_tracing::warn!(
                component = "mcp",
                operation = "search_candidates",
                rows = saved.len(),
                views_max = SLOTS_MAX,
                ?retained,
                bytes_max = BYTES_MAX,
                ?stored_bytes,
                ?stored_capacity,
                incoming_bytes = encoded.len(),
                incoming_capacity = encoded.capacity(),
                duplicate = saved.iter().any(|row| row.id == id),
                "captured candidates exceed the byte or view bound"
            );
            return Err(errors::server::read_unavailable()
                .operation("search_candidates")
                .detail("captured candidates exceed the byte or view bound")
                .error());
        }
        saved.push(SavedCandidateBytes {
            id,
            expires,
            bytes: encoded,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{BYTES_MAX, CandidateResponses, RiftMcp, SLOTS_MAX, SearchCandidateResult};
    use rmcp::model::CallToolRequestParams;
    use std::time::{Duration, SystemTime};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn request() -> Result<CallToolRequestParams, serde_json::Error> {
        Ok(
            CallToolRequestParams::new("search_candidates").with_arguments(serde_json::from_value(
                serde_json::json!({
                    "params": {"query": "beacon", "scope": "local", "target": "symbol"}
                }),
            )?),
        )
    }

    #[tokio::test]
    async fn preparing_query_refuses_unterminated_quote_without_retaining_candidates() -> TestResult
    {
        let (_directory, assembled) = super::super::tests::starting_fixture().await?;
        let params = serde_json::from_value(serde_json::json!({
            "query": "\"beacon", "scope": "local", "target": "symbol"
        }))?;
        let failure = assembled
            .server
            .discovery_search(
                super::super::Parameters(params),
                rmcp::model::Extensions::default(),
            )
            .await
            .err()
            .expect("preparing publication must refuse the malformed query");
        let super::ToolFailure::Registered(failure, phase) = failure else {
            return Err("query refusal must remain registered".into());
        };
        assert_eq!(phase, super::wire::ErrorPhase::Read);
        assert!(
            failure
                .registered()
                .context()
                .any(|(key, value)| key == "field" && value == "query")
        );
        assert!(
            assembled
                .server
                .candidate_responses
                .bytes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn preparing_retries_allocate_no_candidates_then_capture_ready() -> TestResult {
        let (_directory, assembled) = super::super::tests::starting_fixture().await?;
        for attempt in 0..30 {
            let answer = assembled
                .server
                .search_candidates_request(request()?)
                .await?;
            let value = serde_json::to_value(&answer)?;
            assert_eq!(value["outcome"], "unavailable");
            assert_eq!(value["reason"], "index_preparing");
            assert!(value.get("view").is_none());
            assert!(value.get("results").is_none());
            let warnings = value["warnings"].as_array().ok_or("candidate warnings")?;
            assert!(
                warnings
                    .iter()
                    .any(|warning| warning["code"] == "local_index_preparing")
            );
            assert!(
                warnings
                    .iter()
                    .any(|warning| warning["code"] == "lexical_ranking_unavailable")
            );
            if attempt == 0 {
                let params = serde_json::from_value(serde_json::json!({
                    "query": "beacon", "scope": "local", "target": "symbol"
                }))?;
                let direct = assembled
                    .server
                    .discovery_search(
                        super::super::Parameters(params),
                        rmcp::model::Extensions::default(),
                    )
                    .await
                    .map_err(|failure| format!("{failure:?}"))?
                    .0;
                assert_eq!(serde_json::to_value(direct)?, value);
            }
            assert!(
                assembled
                    .server
                    .candidate_responses
                    .bytes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_empty()
            );
        }
        let initial = crate::validation::discover_initial_workspace(&assembled.context)
            .await?
            .ok_or("initial discovery was cancelled")?;
        crate::validation::prepare_initial_workspace_from(&assembled.context, initial).await?;
        let answer = assembled
            .server
            .search_candidates_request(request()?)
            .await?;
        let SearchCandidateResult::Captured(answer) = answer else {
            return Err("ready publication must capture candidates".into());
        };
        assert!(!answer.inputs.precise.is_empty());
        assert_eq!(
            assembled
                .server
                .candidate_responses
                .bytes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
        let views = assembled.server.local_views.get().ok_or("local views")?;
        let foreign_root = tempfile::tempdir()?;
        assert_eq!(
            views.captured_view(answer.view.id.as_str(), foreign_root.path(), &answer.owner)?,
            Err(rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext)
        );
        let accepted = views.captured_view(
            answer.view.id.as_str(),
            std::path::Path::new(&answer.root),
            &answer.owner,
        )?;
        assert_eq!(accepted, Ok(answer.view));
        Ok(())
    }

    #[tokio::test]
    async fn held_candidates_survive_new_preparation() -> TestResult {
        let (directory, assembled) = super::super::tests::starting_fixture().await?;
        let initial = crate::validation::discover_initial_workspace(&assembled.context)
            .await?
            .ok_or("initial discovery was cancelled")?;
        crate::validation::prepare_initial_workspace_from(&assembled.context, initial).await?;
        let answer = assembled
            .server
            .search_candidates_request(request()?)
            .await?;
        let SearchCandidateResult::Captured(original) = answer else {
            return Err("ready publication must capture candidates".into());
        };
        let pending = RiftMcp::assemble(
            super::super::absolute_root(directory.path())?,
            rift_index::WorkspaceIndexLimits::default(),
            None,
            crate::identity::BuildCheckout::Unversioned,
            crate::validation::unwatched,
            crate::validation::LexicalLane::spawn,
            None,
        )
        .await?;
        let publication = std::sync::Arc::clone(&pending.server.published.read().await.current);
        assert!(publication.preparation.is_some());
        assembled.server.published.write().await.current = publication;
        let params = serde_json::json!({
            "params": {"query": "beacon", "scope": "local", "target": "symbol"},
            "view": original.view
        });
        let request = CallToolRequestParams::new("search_candidates")
            .with_arguments(serde_json::from_value(params)?);
        let selected = assembled.server.search_candidates_request(request).await?;
        let SearchCandidateResult::Captured(selected) = selected else {
            return Err("held view must answer during preparation".into());
        };
        assert_eq!(
            serde_json::to_value(selected)?,
            serde_json::to_value(original)?
        );
        assert_eq!(
            assembled
                .server
                .candidate_responses
                .bytes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
        Ok(())
    }

    #[test]
    fn aggregate_capacity_refusal_preserves_original_candidate() -> TestResult {
        let responses = CandidateResponses::default();
        let id = String::from("first");
        let table_bytes = responses
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .capacity()
            * std::mem::size_of::<super::SavedCandidateBytes>();
        let mut bytes = Vec::with_capacity(BYTES_MAX - table_bytes - id.capacity());
        bytes.push(7);
        let capacity = bytes.capacity();
        let expires = SystemTime::now() + Duration::from_secs(60);
        responses.save_bytes(id, expires, bytes)?;
        assert!(
            responses
                .save_bytes(String::from("next"), expires, vec![8])
                .is_err()
        );
        let saved = responses
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].id, "first");
        assert_eq!(saved[0].bytes, [7]);
        assert_eq!(saved[0].bytes.capacity(), capacity);
        assert_eq!(saved[0].expires, expires);
        Ok(())
    }

    #[test]
    fn candidate_encoding_retains_message_bound() {
        let exact = "x".repeat(BYTES_MAX - 2);
        let accepted = crate::proxy::encode_bounded(&exact, BYTES_MAX);
        assert!(accepted.is_ok());
        if let Ok(encoded) = accepted {
            assert_eq!(encoded.len(), BYTES_MAX);
        }
        let over = "x".repeat(BYTES_MAX - 1);
        assert!(crate::proxy::encode_bounded(&over, BYTES_MAX).is_err());
    }

    #[test]
    fn duplicate_slots_and_expiry_preserve_advertised_candidates() -> TestResult {
        let responses = CandidateResponses::default();
        let expires = SystemTime::now() + Duration::from_secs(60);
        responses.save_bytes(String::from("expired"), SystemTime::UNIX_EPOCH, vec![0])?;
        for serial in 0..SLOTS_MAX {
            responses.save_bytes(format!("view-{serial}"), expires, vec![1])?;
        }
        assert!(
            responses
                .save_bytes(String::from("view-0"), expires, vec![2])
                .is_err()
        );
        assert!(
            responses
                .save_bytes(String::from("overflow"), expires, vec![3])
                .is_err()
        );
        let saved = responses
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(saved.len(), SLOTS_MAX);
        assert_eq!(saved[0].id, "view-0");
        assert_eq!(saved[0].bytes, [1]);
        assert!(saved.iter().all(|row| row.expires == expires));
        assert!(
            saved
                .iter()
                .all(|row| row.id != "expired" && row.id != "overflow")
        );
        Ok(())
    }
}
