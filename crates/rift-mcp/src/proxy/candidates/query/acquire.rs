//! Authoritative inputs are acquired before the proxy evaluates one query.

use super::super::RiftProxy;
use crate::server::{QueryFactRequest, QueryFactResult, QueryFactSelection};
use rift_error::{RiftError, errors};
use rift_protocol::read::{SearchParams, SearchScope};
use rift_search::sparql::{FactRequirements, QueryPlan};
use rmcp::handler::server::tool::IntoCallToolResult as _;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResponse, CallToolResult, ClientRequest,
    ServerResult,
};

#[cfg(test)]
mod tests;

pub(super) enum Acquired {
    Facts(
        Vec<(
            String,
            QueryFactSelection,
            rift_search::sparql::QueryServices,
            Option<rift_search::sparql::QueryIdentities>,
        )>,
    ),
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
        let retained = match self
            .retained_query_inputs(plan, budget, requests, deadline)
            .await?
        {
            Some(Acquired::Facts(facts)) => Some(facts),
            Some(response @ Acquired::Response(_)) => return Ok(response),
            None => None,
        };
        if retained.is_some()
            && plan.retrievals().is_empty()
            && plan.requirements().datasets.is_empty()
            && plan.requirements().source_reads.is_empty()
        {
            return Ok(Acquired::Facts(retained.unwrap_or_default()));
        }
        let mut current = self
            .current_query_inputs(plan, retained.is_none(), budget, requests, deadline)
            .await?;
        if let (Some(mut held), Acquired::Facts(fresh)) = (retained, &mut current) {
            append_selections(fresh, &mut held, &mut budget.clone())?;
        }
        Ok(current)
    }

    async fn current_query_inputs(
        &self,
        plan: &QueryPlan,
        include_domain: bool,
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
                    selection.source_reads.clear();
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
        if include_domain
            && (plan
                .requirements()
                .requires(rift_search::query::FactRequirement::Domain)
                || plan.requirements().selects_node_sources())
        {
            let selected = selections
                .entry("local".to_owned())
                .or_insert_with(|| domain_requirements(plan.requirements()));
            if !selected
                .datasets
                .iter()
                .any(|graph| graph == rift_search::sparql::LOCAL_GRAPH)
            {
                selected
                    .datasets
                    .push(rift_search::sparql::LOCAL_GRAPH.to_owned());
            }
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
            let selected = selections
                .entry(name.to_owned())
                .or_insert_with(|| domain_requirements(plan.requirements()));
            if !selected
                .datasets
                .iter()
                .any(|graph| graph == rift_search::sparql::LOCAL_GRAPH)
            {
                selected
                    .datasets
                    .push(rift_search::sparql::LOCAL_GRAPH.to_owned());
            }
        }
        let mut acquired = Vec::new();
        for (name, requirements) in selections {
            match self
                .query_input(
                    &name,
                    requirements,
                    (None, None),
                    budget,
                    requests,
                    deadline,
                )
                .await?
            {
                Acquired::Facts(mut facts) => {
                    append_selections(&mut acquired, &mut facts, &mut budget.clone())?;
                }
                response @ Acquired::Response(_) => return Ok(response),
            }
        }
        for request in &plan.requirements().source_reads {
            match self
                .source_query_input(request, budget, requests, deadline)
                .await?
            {
                Acquired::Facts(mut facts) => {
                    append_selections(&mut acquired, &mut facts, &mut budget.clone())?;
                }
                response @ Acquired::Response(_) => return Ok(response),
            }
        }
        Ok(Acquired::Facts(acquired))
    }

    pub(super) async fn query_input(
        &self,
        name: &str,
        requirements: FactRequirements,
        selected: (
            Option<rift_protocol::symbol_read::CapturedViewId>,
            Option<crate::global::candidates::SavedCandidateBackend>,
        ),
        budget: &rift_server::QueryFactBudget<'static>,
        requests: &rift_server::ProviderRequests,
        deadline: tokio::time::Instant,
    ) -> Result<Acquired, RiftError> {
        let (view, context) = selected;
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
            view,
            requests: maximum,
        });
        let (request, requirements) = self
            .prepare_query_fact_request(request, context, budget, deadline)
            .await?;
        let (peer, _) = backend.leased_peer(None).await?;
        requests.claim()?;
        // A reconnect cannot repeat unknown nested work under one allowance.
        let response = tokio::time::timeout_at(
            deadline,
            backend.answered(
                &peer,
                ClientRequest::CallToolRequest(CallToolRequest::new(request)),
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
                        if selected.is_empty() {
                            return Err(super::retained::unavailable());
                        }
                        let mut used = 0u64;
                        for body in &selected {
                            budget.check()?;
                            validate_selection(
                                body,
                                &expected_root,
                                &expected_owner,
                                maximum,
                                std::time::SystemTime::now(),
                            )?;
                            body.validate_coverage(&requirements)?;
                            used = used
                                .checked_add(body.requests)
                                .filter(|used| *used <= maximum)
                                .ok_or_else(super::retained::unavailable)?;
                        }
                        validate_graph_disposition(&selected, &requirements)?;
                        validate_targets(&selected)?;
                        allowance.complete(used)?;
                        budget.charge(&selected)?;
                        Ok(Ok(selected))
                    }
                    QueryFactResult::Unavailable(reason) => Ok(Err(reason)),
                }
            })
            .await?;
        match selected {
            Ok(selected) => {
                let mut admission = budget.clone();
                admit_selected_facts(name, selected, &mut admission)
            }
            Err(reason) => Ok(unavailable_response(reason)),
        }
    }

    async fn prepare_query_fact_request(
        &self,
        request: rift_server::QueryExchange<QueryFactRequest>,
        context: Option<crate::global::candidates::SavedCandidateBackend>,
        budget: &rift_server::QueryFactBudget<'static>,
        deadline: tokio::time::Instant,
    ) -> Result<(CallToolRequestParams, FactRequirements), RiftError> {
        let (arguments, requirements) = self
            .query_work(budget, deadline, move |budget| {
                budget.charge(&request)?;
                let arguments = serde_json::to_value(&request)
                    .map_err(encoding_failure)?
                    .as_object()
                    .cloned()
                    .ok_or_else(super::retained::unavailable)?;
                Ok((arguments, request.into_payload()?.requirements))
            })
            .await?;
        let mut request = CallToolRequestParams::new("query_facts").with_arguments(arguments);
        if let Some(context) = &context {
            request = crate::proxy::candidates::read::with_backend_context(request, context);
        }
        let request = self
            .query_work(budget, deadline, move |budget| {
                budget.charge(&request)?;
                Ok(request)
            })
            .await?;
        Ok((request, requirements))
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

fn admit_selected_facts(
    name: &str,
    selected: Vec<QueryFactSelection>,
    admission: &mut rift_server::QueryFactBudget<'_>,
) -> Result<Acquired, RiftError> {
    let mut facts = Vec::new();
    admission.charge_material(
        selected
            .len()
            .checked_mul(size_of::<(
                String,
                QueryFactSelection,
                rift_search::sparql::QueryServices,
                Option<rift_search::sparql::QueryIdentities>,
            )>())
            .ok_or_else(super::retained::unavailable)?,
    )?;
    facts
        .try_reserve_exact(selected.len())
        .map_err(|_| super::retained::unavailable())?;
    for body in selected {
        admission.charge(&name)?;
        admission.charge(&body.services)?;
        let services = body.services.clone();
        facts.push((name.to_owned(), body, services, None));
    }
    Ok(Acquired::Facts(facts))
}

fn domain_requirements(requirements: &FactRequirements) -> FactRequirements {
    let mut selected = requirements.clone();
    selected.retrievals.clear();
    selected.datasets.clear();
    selected.services.clear();
    selected.source_reads.clear();
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
    let response = serde_json::from_value(response.into_payload()?).map_err(|_| incompatible())?;
    if matches!(&response, QueryFactResult::Facts(selected) if selected.is_empty()) {
        return Err(incompatible());
    }
    Ok(response)
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
        || selected
            .graph
            .as_deref()
            .is_some_and(|graph| graph != rift_search::sparql::LOCAL_GRAPH)
    {
        return Err(super::retained::unavailable());
    }
    selected.facts.validate_providers()?;
    Ok(())
}

fn validate_targets(selected: &[QueryFactSelection]) -> Result<(), RiftError> {
    for body in selected {
        for service in &body.services.retrievals {
            if service.targets.len() != service.result.results.len() {
                return Err(super::retained::unavailable());
            }
            for (hit, target) in service.result.results.iter().zip(&service.targets) {
                match (&hit.hit, target) {
                    (
                        rift_protocol::read::SearchHitTarget::Symbol { symbol },
                        Some((owner, endpoint)),
                    ) => {
                        let selected = selected
                            .get(*owner)
                            .ok_or_else(super::retained::unavailable)?;
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

pub(super) fn append_selections(
    selected: &mut Vec<(
        String,
        QueryFactSelection,
        rift_search::sparql::QueryServices,
        Option<rift_search::sparql::QueryIdentities>,
    )>,
    added: &mut Vec<(
        String,
        QueryFactSelection,
        rift_search::sparql::QueryServices,
        Option<rift_search::sparql::QueryIdentities>,
    )>,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<(), RiftError> {
    let offset = selected.len();
    for (_, body, services, _) in added.iter_mut() {
        for services in [&mut body.services, services] {
            for service in &mut services.retrievals {
                for (owner, _) in service.targets.iter_mut().flatten() {
                    *owner = owner
                        .checked_add(offset)
                        .ok_or_else(super::retained::unavailable)?;
                }
            }
        }
    }
    budget.charge_material(
        added
            .len()
            .checked_mul(size_of::<(
                String,
                QueryFactSelection,
                rift_search::sparql::QueryServices,
                Option<rift_search::sparql::QueryIdentities>,
            )>())
            .ok_or_else(super::retained::unavailable)?,
    )?;
    selected
        .try_reserve_exact(added.len())
        .map_err(|_| super::retained::unavailable())?;
    selected.append(added);
    Ok(())
}

pub(super) fn unavailable_response(
    reason: rift_protocol::search_read::SearchUnavailable,
) -> Acquired {
    Acquired::Response(Box::new(
        crate::output::Json(rift_protocol::query::QueryReadResult::Unavailable(reason))
            .into_call_tool_result(),
    ))
}

fn validate_graph_disposition(
    selected: &[QueryFactSelection],
    requirements: &FactRequirements,
) -> Result<(), RiftError> {
    let named = selected
        .iter()
        .filter(|body| body.graph.as_deref() == Some(rift_search::sparql::LOCAL_GRAPH))
        .count();
    let expected = (requirements.retrievals.is_empty() && requirements.source_reads.is_empty())
        || requirements
            .datasets
            .iter()
            .any(|graph| graph == rift_search::sparql::LOCAL_GRAPH);
    if named != usize::from(expected)
        || selected.iter().any(|body| {
            body.graph
                .as_ref()
                .is_some_and(|graph| graph != rift_search::sparql::LOCAL_GRAPH)
        })
    {
        return Err(super::retained::unavailable());
    }
    Ok(())
}
