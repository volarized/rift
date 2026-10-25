use super::RiftProxy;
use rift_error::{RiftError, errors};
use rift_protocol::query::QueryParams;
use rift_search::sparql::QueryLimits;
use rmcp::model::{CallToolRequestParams, CallToolResponse};

mod acquire;
mod evaluate;
mod identity;
mod retained;
mod source;
#[cfg(test)]
mod tests;

pub(super) struct QueryForward {
    pub(super) backend: String,
    pub(super) response: Result<CallToolResponse, rmcp::ErrorData>,
    pub(super) owned: bool,
}

impl RiftProxy {
    pub(super) async fn query_forward(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<QueryForward, RiftError> {
        let params: QueryParams =
            serde_json::from_value(request.arguments.clone().unwrap_or_default().into())
                .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        let worker = self.query_worker.as_ref().ok_or_else(unsupported)?;
        let program = self.query_program.clone().ok_or_else(unsupported)?;
        let limits = QueryLimits::try_from(&self.query_configuration)?;
        let deadline = tokio::time::Instant::now() + self.forward_budget;
        let cancellation = tokio_util::sync::CancellationToken::new();
        let guard = cancellation.clone().drop_guard();
        let plan = tokio::time::timeout_at(
            deadline,
            worker.run_with_cancellation("query parse", cancellation, move |cancelled| {
                rift_server::prepare_query(
                    program.as_path(),
                    params.query,
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
        if let Some(page) = plan.page_selection() {
            if let Some(response) = self.query_material_page(page.clone(), deadline).await? {
                return Ok(owned(Ok(response)));
            }
            let view = rift_protocol::symbol_read::CapturedViewId::parse(&page.view)?;
            if let Ok(Some(selected)) = self
                .candidate_views
                .backend(&view, std::time::SystemTime::now())?
            {
                let backend = self.query_backend(&selected.backend)?;
                return Ok(QueryForward {
                    backend: selected.backend,
                    response: backend.forward_read(request.clone()).await?,
                    owned: false,
                });
            }
        }
        let mut federated = false;
        federated |= plan
            .requirements()
            .datasets
            .iter()
            .any(|graph| graph != rift_search::sparql::LOCAL_GRAPH);
        for producer in plan.retrievals() {
            let backends = self.candidate_backends(producer)?;
            federated |= backends.len() != 1 || backends[0].0 != "local";
        }
        if !federated {
            return Ok(QueryForward {
                backend: "local".to_owned(),
                response: self.forward_read(request.clone()).await?,
                owned: false,
            });
        }
        Ok(owned(
            self.evaluate_owned_query(plan, limits, deadline).await?,
        ))
    }

    fn query_backend(&self, name: &str) -> Result<&Self, RiftError> {
        if name == "local" {
            Ok(self)
        } else {
            self.backends
                .get(name)
                .ok_or_else(|| self.extra.refusal(name, "search", "view"))
        }
    }
}

fn owned(response: Result<CallToolResponse, rmcp::ErrorData>) -> QueryForward {
    QueryForward {
        backend: "local".to_owned(),
        response,
        owned: true,
    }
}

fn unsupported() -> RiftError {
    errors::server::read_unsupported()
        .capability("query execution requires a compatible Rift executable")
        .error()
}
