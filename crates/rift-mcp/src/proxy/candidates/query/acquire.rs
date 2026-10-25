//! Authoritative inputs are acquired before the proxy evaluates one query.

use super::super::RiftProxy;
use crate::server::{QueryFactRequest, QueryFactResult, QueryFactSelection};
use rift_error::{RiftError, errors};
use rift_protocol::read::{SearchParams, SearchScope};
use rift_search::sparql::{FactRequirements, QueryPlan};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResponse, ClientRequest, ServerResult,
};

pub(super) enum Acquired {
    Facts(Vec<(String, QueryFactSelection)>),
    Response(Result<CallToolResponse, rmcp::ErrorData>),
}

impl RiftProxy {
    pub(super) async fn query_inputs(
        &self,
        plan: &QueryPlan,
        budget: &rift_server::QueryFactBudget<'static>,
        requests: &rift_server::ProviderRequests,
        deadline: tokio::time::Instant,
    ) -> Result<Acquired, RiftError> {
        let mut selections: std::collections::BTreeMap<String, FactRequirements> =
            std::collections::BTreeMap::new();
        for producer in plan.retrievals() {
            for (name, _) in self.candidate_backends(producer)? {
                let selection = selections.entry(name.to_owned()).or_insert_with(|| {
                    let mut selection = plan.requirements().clone();
                    selection.retrievals.clear();
                    selection.datasets.clear();
                    selection.services.clear();
                    selection
                });
                let mut producer = producer.clone();
                if name != "local" {
                    producer.scope = SearchScope::Local;
                    producer.packages.clear();
                }
                if !selection.retrievals.contains(&producer) {
                    selection.retrievals.push(producer);
                }
            }
        }
        let mut acquired = Vec::new();
        for (name, requirements) in selections {
            match self
                .query_input(&name, requirements, budget, requests, deadline)
                .await?
            {
                Acquired::Facts(mut facts) => acquired.append(&mut facts),
                response => return Ok(response),
            }
        }
        Ok(Acquired::Facts(acquired))
    }

    async fn query_input(
        &self,
        name: &str,
        requirements: FactRequirements,
        budget: &rift_server::QueryFactBudget<'static>,
        requests: &rift_server::ProviderRequests,
        deadline: tokio::time::Instant,
    ) -> Result<Acquired, RiftError> {
        let backend = if name == "local" {
            self
        } else {
            self.backends
                .get(name)
                .ok_or_else(|| self.extra.refusal(name, "search", "scope"))?
        };
        let expected_root = backend
            .accepted_root
            .as_deref()
            .and_then(std::path::Path::to_str)
            .ok_or_else(super::retained::unavailable)?
            .to_owned();
        let allowance = requests.reserve(requests.remaining().saturating_sub(1))?;
        let maximum = allowance.maximum();
        let request = rift_server::QueryExchange::new(QueryFactRequest {
            requirements,
            view: None,
            requests: maximum,
        });
        let arguments = self
            .query_work(budget, deadline, move |budget| {
                budget.charge(&request)?;
                serde_json::to_value(request)
                    .map_err(encoding_failure)?
                    .as_object()
                    .cloned()
                    .ok_or_else(super::retained::unavailable)
            })
            .await?;
        let (peer, _) = backend.leased_peer(None).await?;
        requests.claim()?;
        // A reconnect cannot repeat unknown nested work under one allowance.
        let response = tokio::time::timeout_at(
            deadline,
            backend.answered(
                &peer,
                ClientRequest::CallToolRequest(CallToolRequest::new(
                    CallToolRequestParams::new("query_facts").with_arguments(arguments),
                )),
            ),
        )
        .await
        .map_err(|_| deadline_failure())??;
        let result = match response {
            Ok(ServerResult::CallToolResult(result)) => result,
            Ok(_) => return Err(super::retained::unavailable()),
            Err(error) => {
                return Ok(Acquired::Response(Err(crate::proxy::forwarded_error(
                    error,
                )?)));
            }
        };
        if result.is_error == Some(true) {
            return Ok(Acquired::Response(Ok(result.into())));
        }
        let selected = self
            .query_work(budget, deadline, move |budget| {
                budget.charge(&result)?;
                let value = result
                    .structured_content
                    .ok_or_else(super::retained::unavailable)?;
                let response: rift_server::QueryExchange<serde_json::Value> =
                    serde_json::from_value(value).map_err(encoding_failure)?;
                let response: QueryFactResult = serde_json::from_value(response.into_payload()?)
                    .map_err(|_| {
                        errors::server::read_unsupported()
                            .capability("query fact response schema is incompatible")
                            .error()
                    })?;
                match response {
                    QueryFactResult::Facts(selected) => {
                        if selected.root != expected_root
                            || selected.selection.owner != selected.owner
                            || crate::proxy::captured_expiry(&selected.view)?
                                <= std::time::SystemTime::now()
                        {
                            return Err(super::retained::unavailable());
                        }
                        selected.facts.validate_providers()?;
                        if selected.requests > maximum {
                            return Err(super::retained::unavailable());
                        }
                        allowance.complete(selected.requests)?;
                        budget.charge(&selected)?;
                        Ok(Ok(*selected))
                    }
                    QueryFactResult::Unavailable(reason) => Ok(Err(reason)),
                }
            })
            .await?;
        match selected {
            Ok(selected) => Ok(Acquired::Facts(vec![(name.to_owned(), selected)])),
            Err(reason) => Ok(Acquired::Response(Ok(
                rmcp::model::CallToolResult::structured(
                    serde_json::to_value(rift_protocol::query::QueryReadResult::Unavailable(
                        reason,
                    ))
                    .map_err(encoding_failure)?,
                )
                .into(),
            ))),
        }
    }

    pub(super) async fn query_work<T: Send + 'static>(
        &self,
        budget: &rift_server::QueryFactBudget<'static>,
        deadline: tokio::time::Instant,
        work: impl FnOnce(&mut rift_server::QueryFactBudget<'_>) -> Result<T, RiftError>
        + Send
        + 'static,
    ) -> Result<T, RiftError> {
        let worker = self.query_worker.as_ref().ok_or_else(super::unsupported)?;
        let budget = budget.clone();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let guard = cancellation.clone().drop_guard();
        let result = tokio::time::timeout_at(
            deadline,
            worker.run_with_cancellation("query acquisition", cancellation, move |cancelled| {
                let cancelled = || cancelled.is_cancelled();
                let mut budget = budget.with_cancellation(&cancelled);
                budget.check()?;
                let value = work(&mut budget)?;
                budget.check()?;
                Ok(value)
            }),
        )
        .await
        .map_err(|_| deadline_failure())?;
        drop(guard);
        result
    }
}

pub(super) fn original_request(
    selected: &SearchParams,
    backend: &str,
    originals: &[SearchParams],
) -> Result<SearchParams, RiftError> {
    originals
        .iter()
        .find(|original| {
            let selected_backend = match &original.scope {
                SearchScope::NamedLocal { name } => backend == name,
                SearchScope::ProfileLocal { name } => {
                    backend.strip_prefix('#') == Some(name.as_str())
                }
                SearchScope::Local | SearchScope::Global => backend == "local",
                SearchScope::All => true,
            };
            if !selected_backend {
                return false;
            }
            let mut request = (*original).clone();
            if backend != "local" {
                request.scope = SearchScope::Local;
                request.packages.clear();
            }
            request == *selected
        })
        .cloned()
        .ok_or_else(super::retained::unavailable)
}

fn deadline_failure() -> RiftError {
    errors::server::read_unavailable()
        .operation("query acquisition")
        .detail("query acquisition exceeded the request deadline")
        .error()
}

fn encoding_failure(source: serde_json::Error) -> RiftError {
    errors::mcp::forward_failed().source(source).error()
}
