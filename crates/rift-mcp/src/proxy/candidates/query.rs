use super::{CapturedCandidates, RiftProxy, invalid};
use rift_error::{RiftError, errors};
use rift_protocol::query::QueryParams;
use rift_search::sparql::{QueryLimits, QueryServiceData};
use rmcp::model::{CallToolRequestParams, CallToolResponse};

pub(super) struct QueryForward {
    pub(super) backend: String,
    pub(super) response: Result<CallToolResponse, rmcp::ErrorData>,
    pub(super) captured: Vec<CapturedCandidates>,
}

impl RiftProxy {
    pub(super) async fn query_forward(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<QueryForward, RiftError> {
        let params: QueryParams =
            serde_json::from_value(request.arguments.clone().unwrap_or_default().into())
                .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        let worker = self.query_worker.as_ref().ok_or_else(|| unsupported())?;
        let program = self.query_program.clone().ok_or_else(|| unsupported())?;
        let limits = QueryLimits::try_from(&self.query_configuration)?;
        let deadline = tokio::time::Instant::now() + self.forward_budget;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let guard = cancellation.clone().drop_guard();
        let statement = params.query.clone();
        let plan = tokio::time::timeout_at(
            deadline,
            worker.run_with_cancellation("query parse", cancellation, move |cancelled| {
                rift_server::prepare_query(
                    program.as_path(),
                    statement,
                    limits,
                    deadline.into_std(),
                    &|| cancelled.is_cancelled(),
                )
            }),
        )
        .await
        .map_err(|_| {
            errors::server::read_unavailable()
                .operation("query parse")
                .detail("query parsing exceeded the request deadline")
                .error()
        })??;
        drop(guard);

        if let Some(page) = plan.page_selection()
            && let view = rift_protocol::symbol_read::CapturedViewId::parse(&page.view)?
            && let Ok(Some(selected)) = self
                .candidate_views
                .backend(&view, std::time::SystemTime::now())?
        {
            let backend = if selected.backend == "local" {
                self
            } else {
                self.backends
                    .get(&selected.backend)
                    .ok_or_else(|| self.extra.refusal(&selected.backend, "search", "view"))?
            };
            return Ok(QueryForward {
                backend: selected.backend,
                response: backend.forward_read(request.clone()).await?,
                captured: Vec::new(),
            });
        }

        let mut federated = false;
        for producer in plan.retrievals() {
            let backends = self.candidate_backends(producer)?;
            federated |= backends.len() != 1 || backends[0].0 != "local";
        }
        if !federated {
            return Ok(QueryForward {
                backend: "local".to_owned(),
                response: self.forward_read(request.clone()).await?,
                captured: Vec::new(),
            });
        }
        if plan.requirements().domain {
            return Err(errors::server::read_unavailable()
                .operation("query acquisition")
                .detail("required graph facts are unavailable for the selected projects")
                .error());
        }
        let mut retrievals = Vec::new();
        let mut captured = Vec::new();
        let mut selected = None;
        for producer in plan.retrievals() {
            let (result, context) = match self.query_retrieval(producer).await? {
                Ok(result) => result,
                Err(response) => {
                    return Ok(QueryForward {
                        backend: "local".to_owned(),
                        response: Ok(response.into()),
                        captured,
                    });
                }
            };
            let first = context
                .pools
                .iter()
                .find(|pool| pool.backend != "global")
                .ok_or_else(|| invalid("scope", "captured project context is unavailable"))?;
            if selected
                .as_ref()
                .is_some_and(|selected| selected != &first.backend)
            {
                return Err(invalid("scope", "captured project contexts do not match"));
            }
            selected = Some(first.backend.clone());
            retrievals.push(QueryServiceData {
                request: producer.clone(),
                result,
            });
            captured.push(context);
        }
        let selected =
            selected.ok_or_else(|| invalid("scope", "captured project context is unavailable"))?;
        let backend = if selected == "local" {
            self
        } else {
            self.backends
                .get(&selected)
                .ok_or_else(|| self.extra.refusal(&selected, "search", "scope"))?
        };
        let supplied = crate::server::QueryCandidateRequest { params, retrievals };
        super::super::encode_bounded(&supplied, super::CANDIDATE_BYTES_MAX)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        let arguments = serde_json::to_value(supplied)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?
            .as_object()
            .cloned()
            .ok_or_else(|| invalid("arguments", "query request is not an object"))?;
        let forwarded = CallToolRequestParams::new("query_candidates").with_arguments(arguments);
        let response = tokio::time::timeout_at(deadline, backend.forward_read(forwarded))
            .await
            .map_err(|_| {
                errors::server::read_unavailable()
                    .operation("query acquisition")
                    .detail("query acquisition exceeded the request deadline")
                    .error()
            })??;
        Ok(QueryForward {
            backend: selected,
            response,
            captured,
        })
    }
}

fn unsupported() -> RiftError {
    errors::server::read_unsupported()
        .capability("query execution requires a compatible Rift executable")
        .error()
}
