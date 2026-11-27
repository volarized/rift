mod graph_query;
pub(crate) use graph_query::EngineInventoryCache;
pub(crate) use graph_query::{
    QueryPageOptions, query_page,
    retrieval::{bound_combined_rows, collect_retrieval, retrieval_coverage_for},
};
mod symbol;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use rift_error::{RiftError, errors};
use rift_protocol::read::{ReadWarning, SearchParams};
use rift_protocol::symbol_read::{CapturedView, SymbolUnavailableReason};
use rift_server::SearchCandidateInputs;
use rmcp::handler::server::tool::IntoCallToolResult as _;
use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult, MetaObject};

use crate::failure::{McpErrorExt as _, WireFailure as _};
use serde::{Deserialize, Serialize};

use super::{ReadAnswer, RiftMcp, ToolFailure, wire};

const BYTES_MAX: usize = crate::global::candidates::CANDIDATE_BYTES_MAX;
/// The producer's typed coverage outcome on a peer read.
pub(crate) const RETRIEVAL_COVERAGE_KEY: &str = "rift/retrieval-coverage";
// The default `[search.query] retained_views`, the rows' initial capacity.
const SLOTS_DEFAULT: usize = 256;
// The largest `[search.query] retained_views`, `QUERY_RETAINED_VIEWS_MAX`; a test pins it.
const RELEASED_KEYS_MAX: usize = 4096;

pub(super) fn set_retrieval_coverage(
    result: &mut CallToolResult,
    reason: Option<SymbolUnavailableReason>,
) {
    if let Some(reason) = reason {
        let value = serde_json::to_value(reason)
            .unwrap_or_else(|error| unreachable!("a retrieval outcome serializes: {error}"));
        result
            .meta
            .get_or_insert_with(MetaObject::new)
            .insert(RETRIEVAL_COVERAGE_KEY.to_owned(), value);
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchCandidateRequest {
    pub(crate) params: SearchParams,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) view: Option<CapturedView>,
    #[serde(default)]
    pub(crate) require_ready_ranking: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchCandidateResponse {
    pub(crate) root: String,
    pub(crate) owner: rift_protocol::identity::SymbolOwner,
    pub(crate) view: CapturedView,
    /// The revision the captured candidates read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) revision: Option<rift_protocol::revision::Revision>,
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

/// Captured candidate answers in least-recent-use order, oldest first.
#[derive(Debug)]
pub(crate) struct CandidateResponses {
    bytes: Mutex<Vec<SavedCandidateBytes>>,
    /// Views whose rows were released to admit newer ones; locked after `bytes` only.
    released: Mutex<std::collections::BTreeSet<String>>,
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
            bytes: Mutex::new(Vec::with_capacity(SLOTS_DEFAULT)),
            released: Mutex::default(),
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
    lexical_commit_pending: bool,
) -> Option<rift_protocol::search_read::SearchUnavailable> {
    let warning = resolved
        .published
        .preparation
        .as_ref()
        .and_then(crate::validation::LocalIndexPreparation::warning);
    if warning.is_none() && !lexical_commit_pending {
        return None;
    }
    let mut answer_warnings = warnings.to_vec();
    answer_warnings.extend(resolved.stale.clone());
    answer_warnings.extend(warning);
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
        require_ready_ranking: bool,
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
        if params.traversal.is_some()
            || params.pattern.is_some()
            || params.change.is_some()
            || params.revision.is_some()
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
                let read = self.candidate_responses.read_used(view.as_str(), |row| {
                    serde_json::from_slice::<SavedResponse>(&row.bytes)
                        .map(|saved| (saved, row.expires))
                });
                let Some(read) = read else {
                    let reason = self.candidate_responses.missing_reason(view.as_str());
                    return Ok(Some(unavailable(reason, None)));
                };
                let (saved, expires) = read.map_err(transport_failure)?;
                if expires <= SystemTime::now() {
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
            require_ready_ranking,
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
            revision: captured.revision,
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
        let current = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("view"))
            .is_none_or(serde_json::Value::is_null);
        let result = self.search_candidates_request(request).await;
        match result {
            Ok(mut answer) => {
                use rift_protocol::search_read::SearchUnavailable;
                let coverage = match &mut answer {
                    SearchCandidateResult::Captured(response) => {
                        let warnings = response.inputs.warnings.iter().chain(
                            response
                                .global
                                .iter()
                                .flat_map(|pool| &pool.inputs.warnings),
                        );
                        let coverage = retrieval_coverage_for(warnings, current);
                        crate::output::record_warnings(&mut response.inputs.warnings);
                        if let Some(pool) = &mut response.global {
                            crate::output::record_warnings(&mut pool.inputs.warnings);
                        }
                        coverage
                    }
                    SearchCandidateResult::Unavailable(SearchUnavailable::Unavailable {
                        warnings,
                        ..
                    }) => {
                        crate::output::record_warnings(warnings);
                        None
                    }
                };
                let value = serde_json::to_value(answer).map_err(|source| {
                    let error = errors::mcp::forward_failed().source(source).error();
                    operation.record("error.type", error.slug().as_str());
                    error.mcp().tool_error(wire::ErrorPhase::Read)
                })?;
                let mut result = CallToolResult::structured(value);
                set_retrieval_coverage(&mut result, coverage);
                Ok(result.into())
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
            return self.held_candidates(views, &requested.params, view, SystemTime::now());
        }
        validate_candidate_query(&requested.params)?;
        let (resolved, ranking, references, deadline) =
            self.current_search_inputs(&requested.params).await?;
        if let Some(answer) = preparing_candidates(
            &resolved,
            &ranking.warnings,
            requested.require_ready_ranking && ranking.lexical_commit_pending,
        ) {
            return Ok(SearchCandidateResult::Unavailable(answer));
        }
        let answer = ranking.answer;
        let warnings = ranking.warnings;
        let ready = resolved.published.preparation.is_none();
        let params = requested.params.clone();
        let (mut inputs, material) = self
            .current_captured_read(&resolved, true, move |reads, capture_warnings| {
                let (mut inputs, material) =
                    reads.search_candidate_view_input(&params, &answer, &references, ready)?;
                inputs.warnings.extend(capture_warnings.iter().cloned());
                let material = material.with_warnings(capture_warnings)?;
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
        let revision = resolved
            .published
            .reads
            .served_revision()
            .map_err(read_failure)?;
        let mut response = None;
        let limits = rift_server::RetainedViewLimits::from(&configuration.query);
        views
            .capture_with(material, limits, |view| {
                let answer = SearchCandidateResponse {
                    root,
                    owner,
                    view: view.clone(),
                    revision: Some(revision.clone()),
                    inputs,
                    ranking: configuration.ranking,
                    results_max,
                    global,
                };
                self.save_candidates(requested.params, answer.clone(), limits.views_max())?;
                response = Some(answer);
                Ok(true)
            })
            .map_err(read_failure)?;
        response
            .map(|response| SearchCandidateResult::Captured(Box::new(response)))
            .ok_or_else(|| unavailable("retained read material exceeds the byte bound"))
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
            || params.scope.is_project()
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
            params.revision.as_ref(),
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
        views_max: usize,
    ) -> Result<(), RiftError> {
        let id = response.view.id.as_str().to_owned();
        let expires = crate::proxy::captured_expiry(&response.view)?;
        let encoded = crate::proxy::encode_bounded(&SavedResponse { params, response }, BYTES_MAX)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        self.candidate_responses
            .save_bytes(id, expires, encoded, views_max)
    }

    /// Answers the candidates `view` retained as of `now`, or the typed reason a `search`
    /// page naming that view answers: `view_expired` once the view expired, else
    /// `view_context`.
    fn held_candidates(
        &self,
        views: &rift_server::LocalSymbolViews,
        params: &SearchParams,
        view: &CapturedView,
        now: SystemTime,
    ) -> Result<SearchCandidateResult, ToolFailure> {
        use rift_protocol::symbol_read::SymbolUnavailableReason;
        let refused = |reason| {
            SearchCandidateResult::Unavailable(
                rift_protocol::search_read::SearchUnavailable::Unavailable {
                    reason,
                    view: Some(view.clone()),
                    warnings: Vec::new(),
                },
            )
        };
        let response = match self.saved_candidates(params, view, now)? {
            Ok(response) => response,
            Err(reason) => return Ok(refused(reason)),
        };
        let accepted = views.captured_view_at(
            &view.id,
            std::path::Path::new(&response.root),
            &response.owner,
            now,
        );
        match accepted {
            Ok(accepted) if &accepted == view => {
                Ok(SearchCandidateResult::Captured(Box::new(response)))
            }
            Ok(_) => Ok(refused(SymbolUnavailableReason::ViewContext)),
            Err(reason) => Ok(refused(reason)),
        }
    }

    fn saved_candidates(
        &self,
        params: &SearchParams,
        view: &CapturedView,
        now: SystemTime,
    ) -> Result<
        Result<SearchCandidateResponse, rift_protocol::symbol_read::SymbolUnavailableReason>,
        ToolFailure,
    > {
        use rift_protocol::symbol_read::SymbolUnavailableReason;
        let held = self.candidate_responses.read_used(view.id.as_str(), |row| {
            serde_json::from_slice::<SavedResponse>(&row.bytes).map(|saved| (saved, row.expires))
        });
        let Some(held) = held else {
            let expired = crate::proxy::captured_expiry(view).map_err(read_failure)? <= now;
            return Ok(Err(if expired {
                SymbolUnavailableReason::ViewExpired
            } else {
                self.candidate_responses.missing_reason(view.id.as_str())
            }));
        };
        let (saved, expires) = held.map_err(transport_failure)?;
        if expires <= now {
            return Ok(Err(SymbolUnavailableReason::ViewExpired));
        }
        if crate::proxy::normalized_params(&saved.params) != crate::proxy::normalized_params(params)
            || &saved.response.view != view
        {
            return Ok(Err(SymbolUnavailableReason::ViewContext));
        }
        Ok(Ok(saved.response))
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
            "query": "beacon", "scope": "local", "include": {"source": true}
        }))?;
        let initial = server
            .search(super::super::Parameters(params.clone()))
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
        std::fs::write(directory.path().join("lib.rs"), "pub fn replacement() {}\n")?;
        params.view = Some(view.id.clone());
        let later = server
            .search(super::super::Parameters(params.clone()))
            .await
            .map_err(|failure| format!("{failure:?}"))?
            .0;
        let rift_protocol::search_read::SearchReadResult::Results(later) = later else {
            return Err("held search must return original results".into());
        };
        assert_eq!(later, initial);
        params.page = 2;
        let page = server
            .search(super::super::Parameters(params.clone()))
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
                .search(super::super::Parameters(params))
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
            "query": "captured_nonexistent_symbol_329b", "scope": "local",
            "paths": {"include": ["missing.rs"]}
        }))?;
        let request = serde_json::to_value(SearchCandidateRequest {
            params: params.clone(),
            view: None,
            require_ready_ranking: false,
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
        let now = std::time::SystemTime::now();
        let continued = server
            .saved_candidates(&params, &selected, now)
            .map_err(|failure| format!("{failure:?}"))?
            .map_err(|reason| format!("{reason:?}"))?;
        assert_eq!(continued.view, selected);
        let mut changed = params;
        changed.query = Some("beacon".to_owned());
        assert_eq!(
            server
                .saved_candidates(&changed, &selected, now)
                .map_err(|failure| format!("{failure:?}"))?
                .err(),
            Some(rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext)
        );
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
            require_ready_ranking: false,
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
            require_ready_ranking: false,
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
    /// Saves one captured answer, releasing least recently used rows past either bound.
    ///
    /// Refuses only a duplicate view or an answer whose own bytes exceed `BYTES_MAX`;
    /// a released row's later continuation answers `view_expired`. Each release scans the
    /// rows once, at most `views_max` of them.
    fn save_bytes(
        &self,
        id: String,
        expires: SystemTime,
        encoded: Vec<u8>,
        views_max: usize,
    ) -> Result<(), RiftError> {
        let mut saved = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        saved.retain(|row| row.expires > SystemTime::now());
        let duplicate = saved.iter().any(|row| row.id == id);
        let incoming = id.capacity().saturating_add(encoded.capacity());
        let alone = retained_candidate_bytes(&saved[..0], saved.capacity(), incoming);
        if duplicate || alone.is_none_or(|bytes| bytes > BYTES_MAX) {
            refuse_candidates(&saved, &encoded, alone, views_max, duplicate);
            return Err(errors::server::read_unavailable()
                .operation("search_candidates")
                .detail("captured candidates exceed the byte bound or repeat a retained view")
                .error());
        }
        let mut released = Vec::new();
        while !saved.is_empty()
            && (saved.len() >= views_max
                || retained_candidate_bytes(&saved, saved.capacity(), incoming)
                    .is_none_or(|bytes| bytes > BYTES_MAX))
        {
            released.push(saved.remove(0).id);
        }
        saved.push(SavedCandidateBytes {
            id,
            expires,
            bytes: encoded,
        });
        drop(saved);
        self.release(released);
        Ok(())
    }

    /// Records released view ids so a later continuation names its view expired.
    fn release(&self, released: Vec<String>) {
        if released.is_empty() {
            return;
        }
        let mut keys = self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        keys.extend(released);
        while keys.len() > RELEASED_KEYS_MAX {
            keys.pop_first();
        }
    }

    /// Reads one row through `read` and marks it the most recently used.
    ///
    /// `read` runs while the rows are locked and must not call this store.
    fn read_used<T>(&self, id: &str, read: impl FnOnce(&SavedCandidateBytes) -> T) -> Option<T> {
        let mut saved = self
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let position = saved.iter().position(|row| row.id == id)?;
        let row = saved.remove(position);
        let answer = read(&row);
        saved.push(row);
        Some(answer)
    }

    /// Why no row answers `id`: released to admit newer rows, or never retained here.
    fn missing_reason(&self, id: &str) -> rift_protocol::symbol_read::SymbolUnavailableReason {
        use rift_protocol::symbol_read::SymbolUnavailableReason;
        let released = self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(id);
        if released {
            SymbolUnavailableReason::ViewExpired
        } else {
            SymbolUnavailableReason::ViewContext
        }
    }
}

/// Bytes `rows` and an incoming row of `incoming` bytes charge, with the table at
/// `capacity` rows.
fn retained_candidate_bytes(
    rows: &[SavedCandidateBytes],
    capacity: usize,
    incoming: usize,
) -> Option<usize> {
    rows.iter().fold(
        capacity
            .checked_mul(std::mem::size_of::<SavedCandidateBytes>())
            .and_then(|bytes| bytes.checked_add(incoming)),
        |sum, row| {
            sum.and_then(|bytes| bytes.checked_add(row.id.capacity()))
                .and_then(|bytes| bytes.checked_add(row.bytes.capacity()))
        },
    )
}

fn refuse_candidates(
    saved: &[SavedCandidateBytes],
    encoded: &[u8],
    retained: Option<usize>,
    views_max: usize,
    duplicate: bool,
) {
    let stored_bytes = saved
        .iter()
        .try_fold(0usize, |sum, row| sum.checked_add(row.bytes.len()));
    rift_tracing::warn!(
        component = "mcp",
        operation = "search_candidates",
        rows = saved.len(),
        views_max,
        ?retained,
        bytes_max = BYTES_MAX,
        ?stored_bytes,
        incoming_bytes = encoded.len(),
        duplicate,
        "captured candidates exceed the byte bound or repeat a retained view"
    );
}

#[cfg(test)]
mod tests {
    use super::{BYTES_MAX, CandidateResponses, RiftMcp, SLOTS_DEFAULT, SearchCandidateResult};
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
            .search(super::super::Parameters(params))
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
                    .search(super::super::Parameters(params))
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

    /// #766: a continued candidate read names why its view is gone, as `search` pages do.
    #[tokio::test]
    async fn released_candidate_views_answer_typed_reasons() -> TestResult {
        use rift_protocol::symbol_read::SymbolUnavailableReason;
        let (_directory, assembled) = super::super::tests::starting_fixture().await?;
        let initial = crate::validation::discover_initial_workspace(&assembled.context)
            .await?
            .ok_or("initial discovery was cancelled")?;
        crate::validation::prepare_initial_workspace_from(&assembled.context, initial).await?;
        let SearchCandidateResult::Captured(captured) = assembled
            .server
            .search_candidates_request(request()?)
            .await?
        else {
            return Err("ready publication must capture candidates".into());
        };
        let views = assembled.server.local_views.get().ok_or("local views")?;
        let params: super::SearchParams = serde_json::from_value(serde_json::json!({
            "query": "beacon", "scope": "local", "target": "symbol"
        }))?;
        let reason = |answer: SearchCandidateResult| match answer {
            SearchCandidateResult::Unavailable(
                rift_protocol::search_read::SearchUnavailable::Unavailable { reason, view, .. },
            ) => Ok((reason, view)),
            SearchCandidateResult::Captured(_) => Err("a gone view must not answer candidates"),
        };
        let now = SystemTime::now();
        let held = assembled
            .server
            .held_candidates(views, &params, &captured.view, now)
            .map_err(|failure| format!("{failure:?}"))?;
        assert!(matches!(held, SearchCandidateResult::Captured(_)));
        let later = now + Duration::from_mins(16);
        assert_eq!(
            reason(
                assembled
                    .server
                    .held_candidates(views, &params, &captured.view, later)
                    .map_err(|failure| format!("{failure:?}"))?
            )?,
            (
                SymbolUnavailableReason::ViewExpired,
                Some(captured.view.clone())
            )
        );
        let mut changed = params.clone();
        changed.query = Some("other".to_owned());
        assert_eq!(
            reason(
                assembled
                    .server
                    .held_candidates(views, &changed, &captured.view, now)
                    .map_err(|failure| format!("{failure:?}"))?
            )?
            .0,
            SymbolUnavailableReason::ViewContext
        );
        let mut unknown = captured.view.clone();
        unknown.id = rift_protocol::symbol_read::CapturedViewId::parse("unknown.view")?;
        let arguments = serde_json::json!({"params": params, "view": unknown});
        let answer = assembled
            .server
            .search_candidates_request(
                CallToolRequestParams::new("search_candidates")
                    .with_arguments(serde_json::from_value(arguments)?),
            )
            .await
            .map_err(|failure| format!("{failure:?}"))?;
        let value = serde_json::to_value(&answer)?;
        assert_eq!(value["outcome"], "unavailable", "{value}");
        assert_eq!(value["reason"], "view_context", "{value}");
        let mut expired = unknown;
        expired.expires_at = "2000-01-01T00:00:00Z".to_owned();
        assert_eq!(
            reason(
                assembled
                    .server
                    .held_candidates(views, &params, &expired, now)
                    .map_err(|failure| format!("{failure:?}"))?
            )?
            .0,
            SymbolUnavailableReason::ViewExpired
        );
        Ok(())
    }

    #[test]
    fn aggregate_capacity_releases_the_least_recently_used_candidate() -> TestResult {
        use rift_protocol::symbol_read::SymbolUnavailableReason;
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
        let expires = SystemTime::now() + Duration::from_secs(60);
        responses.save_bytes(id, expires, bytes, SLOTS_DEFAULT)?;
        responses.save_bytes(String::from("next"), expires, vec![8], SLOTS_DEFAULT)?;
        let saved = responses
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].id, "next");
        assert_eq!(saved[0].bytes, [8]);
        drop(saved);
        assert_eq!(
            responses.missing_reason("first"),
            SymbolUnavailableReason::ViewExpired
        );
        assert_eq!(
            responses.missing_reason("never"),
            SymbolUnavailableReason::ViewContext
        );
        Ok(())
    }

    #[test]
    fn candidate_bounds_follow_the_retained_view_bounds() {
        let defaults = rift_protocol::query::QueryConfiguration::default();
        assert_eq!(u64::try_from(SLOTS_DEFAULT), Ok(defaults.retained_views));
        assert_eq!(
            u64::try_from(super::RELEASED_KEYS_MAX),
            Ok(rift_protocol::query::QUERY_RETAINED_VIEWS_MAX)
        );
    }

    #[test]
    fn oversized_candidate_refusal_preserves_retained_candidates() -> TestResult {
        let responses = CandidateResponses::default();
        let expires = SystemTime::now() + Duration::from_secs(60);
        responses.save_bytes(String::from("first"), expires, vec![7], SLOTS_DEFAULT)?;
        let oversized = Vec::with_capacity(BYTES_MAX);
        assert!(
            responses
                .save_bytes(String::from("oversized"), expires, oversized, SLOTS_DEFAULT)
                .is_err()
        );
        let saved = responses
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].id, "first");
        assert_eq!(saved[0].bytes, [7]);
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
    fn duplicate_slots_and_expiry_release_the_least_recently_used_candidate() -> TestResult {
        let responses = CandidateResponses::default();
        let expires = SystemTime::now() + Duration::from_secs(60);
        responses.save_bytes(
            String::from("expired"),
            SystemTime::UNIX_EPOCH,
            vec![0],
            SLOTS_DEFAULT,
        )?;
        for serial in 0..SLOTS_DEFAULT {
            responses.save_bytes(format!("view-{serial}"), expires, vec![1], SLOTS_DEFAULT)?;
        }
        assert!(
            responses
                .save_bytes(String::from("view-0"), expires, vec![2], SLOTS_DEFAULT)
                .is_err(),
            "a retained view is never saved twice"
        );
        assert_eq!(
            responses.read_used("view-0", |row| row.bytes.clone()),
            Some(vec![1]),
            "reading the oldest row makes it the most recently used"
        );
        responses.save_bytes(String::from("overflow"), expires, vec![3], SLOTS_DEFAULT)?;
        let saved = responses
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(saved.len(), SLOTS_DEFAULT);
        assert!(
            saved
                .iter()
                .all(|row| row.id != "expired" && row.id != "view-1")
        );
        assert!(saved.iter().any(|row| row.id == "view-0"));
        assert_eq!(saved.last().map(|row| row.id.as_str()), Some("overflow"));
        drop(saved);
        responses.save_bytes(String::from("narrow"), expires, vec![4], 2)?;
        let saved = responses
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ids: Vec<_> = saved.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            ["overflow", "narrow"],
            "a lowered bound releases down to it"
        );
        Ok(())
    }
}
