//! Authoritative inputs are acquired before the proxy evaluates one query.

use super::super::RiftProxy;
use crate::server::{QueryFactRequest, QueryFactResult, QueryFactSelection};
use rift_error::{RiftError, errors};
use rift_protocol::read::{SearchParams, SearchScope};
use rift_search::sparql::{FactRequirements, QueryPlan};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResponse, CallToolResult, ClientRequest,
    ServerResult,
};

#[cfg(test)]
mod tests;

pub(super) enum Acquired {
    Facts(Vec<(String, QueryFactSelection)>),
    Response(Box<Result<CallToolResponse, rmcp::ErrorData>>),
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
        if plan
            .requirements()
            .requires(rift_search::query::FactRequirement::Domain)
        {
            selections
                .entry("local".to_owned())
                .or_insert_with(|| domain_requirements(plan.requirements()));
        }
        for graph in &plan.requirements().datasets {
            let name = if graph == rift_search::sparql::LOCAL_GRAPH {
                "local"
            } else if let Some(name) = graph.strip_prefix(rift_search::sparql::EXTRA_PROJECTS) {
                self.query_backend(name)?;
                name
            } else {
                return Err(super::retained::unavailable());
            };
            selections
                .entry(name.to_owned())
                .or_insert_with(|| domain_requirements(plan.requirements()));
        }
        let mut acquired = Vec::new();
        for (name, requirements) in selections {
            match self
                .query_input(&name, requirements, budget, requests, deadline)
                .await?
            {
                Acquired::Facts(mut facts) => acquired.append(&mut facts),
                response @ Acquired::Response(_) => return Ok(response),
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
        let expected_owner = backend.owner.clone();
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
                return Ok(Acquired::Response(Box::new(Err(
                    crate::proxy::forwarded_error(error)?,
                ))));
            }
        };
        if result.is_error == Some(true) {
            return Ok(Acquired::Response(Box::new(Ok(result.into()))));
        }
        let selected = self
            .query_work(budget, deadline, move |budget| {
                budget.charge(&result)?;
                match decode_response(result)? {
                    QueryFactResult::Facts(selected) => {
                        validate_selection(
                            &selected,
                            &expected_root,
                            &expected_owner,
                            maximum,
                            std::time::SystemTime::now(),
                        )?;
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
            Err(reason) => Ok(Acquired::Response(Box::new(Ok(
                rmcp::model::CallToolResult::structured(
                    serde_json::to_value(rift_protocol::query::QueryReadResult::Unavailable(
                        reason,
                    ))
                    .map_err(encoding_failure)?,
                )
                .into(),
            )))),
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

fn domain_requirements(requirements: &FactRequirements) -> FactRequirements {
    let mut selected = requirements.clone();
    selected.retrievals.clear();
    selected.datasets.clear();
    selected.services.clear();
    selected
}

fn decode_response(result: CallToolResult) -> Result<QueryFactResult, RiftError> {
    let incompatible = || {
        errors::server::read_unsupported()
            .capability("query fact response schema is incompatible")
            .error()
    };
    let response: rift_server::QueryExchange<serde_json::Value> =
        serde_json::from_value(result.structured_content.ok_or_else(incompatible)?)
            .map_err(|_| incompatible())?;
    serde_json::from_value(response.into_payload()?).map_err(|_| incompatible())
}

fn validate_selection(
    selected: &QueryFactSelection,
    root: &str,
    owner: &rift_protocol::identity::SymbolOwner,
    maximum: u64,
    now: std::time::SystemTime,
) -> Result<(), RiftError> {
    let root_uri = rift_server::query_provider_root(std::path::Path::new(root))?;
    if selected.root != root
        || &selected.owner != owner
        || selected.selection.owner != selected.owner
        || selected.selection.root.as_str() != root_uri.as_str()
        || crate::proxy::captured_expiry(&selected.view)? <= now
        || selected.requests > maximum
    {
        return Err(super::retained::unavailable());
    }
    selected.facts.validate_providers()?;
    for service in &selected.services.retrievals {
        if service.targets.len() != service.result.results.len() {
            return Err(super::retained::unavailable());
        }
        for (hit, target) in service.result.results.iter().zip(&service.targets) {
            match (&hit.hit, target) {
                (
                    rift_protocol::read::SearchHitTarget::Symbol { symbol },
                    Some((graph, endpoint)),
                ) => {
                    if graph != rift_search::sparql::LOCAL_GRAPH {
                        return Err(super::retained::unavailable());
                    }
                    let provider = endpoint
                        .provider
                        .and_then(|provider| selected.facts.providers.get(provider))
                        .ok_or_else(super::retained::unavailable)?;
                    let record = provider
                        .record(&endpoint.target)
                        .ok_or_else(super::retained::unavailable)?;
                    if provider.context.selection != selected.selection
                        || selected
                            .facts
                            .symbols
                            .get(record.symbol_index)
                            .is_none_or(|selected| selected.symbol != **symbol)
                    {
                        return Err(super::retained::unavailable());
                    }
                }
                (rift_protocol::read::SearchHitTarget::Symbol { symbol }, None) => {
                    rift_search::query::QueryProviderTarget::canonical(
                        symbol
                            .id
                            .as_ref()
                            .ok_or_else(super::retained::unavailable)?,
                    )?;
                }
                (_, None) => {}
                (_, Some(_)) => return Err(super::retained::unavailable()),
            }
        }
    }
    Ok(())
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
