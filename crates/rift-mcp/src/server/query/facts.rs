//! Authoritative query facts acquired through the existing local read owners.

use rift_error::{RiftError, errors};
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::ReadWarning;
use rift_protocol::search_read::SearchUnavailable;
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};
use rift_search::sparql::{FactRequirements, LOCAL_GRAPH, QueryFacts, QueryServices};
use rift_server::QueryExchange;
use serde::{Deserialize, Serialize};

use super::{
    QueryBudget, QueryExecution, RiftMcp, ToolFailure, acquisition_failure, failure, query_limits,
};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryFactRequest {
    pub(crate) requirements: FactRequirements,
    pub(crate) view: Option<CapturedViewId>,
    pub(crate) requests: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueryFactSelection {
    pub(crate) root: String,
    pub(crate) owner: SymbolOwner,
    pub(crate) view: CapturedView,
    pub(crate) selection: rift_search::query::QueryProviderSelection,
    pub(crate) requests: u64,
    pub(crate) facts: QueryFacts,
    pub(crate) services: QueryServices,
    pub(crate) warnings: Vec<ReadWarning>,
    pub(crate) upstream: Vec<super::super::candidates::QueryGlobalSelection>,
}

#[derive(Deserialize, Serialize)]
#[serde(
    tag = "outcome",
    content = "response",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum QueryFactResult {
    Facts(Box<QueryFactSelection>),
    Unavailable(SearchUnavailable),
}

type SelectedFactView = Option<(rift_server::LocalSymbolViewInput, CapturedView)>;

impl RiftMcp {
    pub(in crate::server) async fn query_facts_call(
        &self,
        request: rmcp::model::CallToolRequestParams,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        use rmcp::handler::server::tool::IntoCallToolResult as _;

        match self.authoritative_query_facts(request).await {
            Ok(response) => Ok(rmcp::model::CallToolResult::structured(response).into()),
            Err(failure) => failure.into_call_tool_result(),
        }
    }

    async fn authoritative_query_facts(
        &self,
        request: rmcp::model::CallToolRequestParams,
    ) -> Result<serde_json::Value, ToolFailure> {
        Box::pin(async move {
            let deadline = self.request_deadline().await;
            let configuration = self.query_configuration().await;
            let limits = query_limits(&configuration)?;
            let budget = QueryBudget::for_request(limits, deadline.at().into_std(), &|| false)
                .map_err(failure)?;
            let request = self
                .decode_query_fact_request(request, &budget, deadline)
                .await?;
            if request
                .requirements
                .datasets
                .iter()
                .any(|dataset| dataset != LOCAL_GRAPH)
            {
                return self
                    .encode_query_fact_response(
                        QueryFactResult::Unavailable(unavailable(
                            SymbolUnavailableReason::ViewContext,
                        )),
                        &budget,
                        deadline,
                    )
                    .await;
            }
            let selected_view = match self.query_fact_view(request.view)? {
                Ok(selected) => selected,
                Err(reason) => {
                    return self
                        .encode_query_fact_response(
                            QueryFactResult::Unavailable(reason),
                            &budget,
                            deadline,
                        )
                        .await;
                }
            };
            let requests =
                rift_server::ProviderRequests::new(configuration.requests.min(request.requests));
            let request_maximum = requests.remaining();
            let selected = match self
                .acquire_query_data(
                    &request.requirements,
                    &request.requirements.retrievals,
                    selected_view,
                    QueryExecution {
                        configuration: &configuration,
                        limits,
                        budget: &budget,
                        deadline,
                        requests: &requests,
                    },
                )
                .await?
            {
                Ok(selected) => selected,
                Err(reason) => {
                    return self
                        .encode_query_fact_response(
                            QueryFactResult::Unavailable(reason),
                            &budget,
                            deadline,
                        )
                        .await;
                }
            };
            let selected = self
                .query_work(&budget, deadline, move |budget| {
                    selected.bind_targets(budget)
                })
                .await?;
            if request
                .requirements
                .validate_facts(&selected.facts)
                .is_err()
            {
                return self
                    .encode_query_fact_response(
                        QueryFactResult::Unavailable(SearchUnavailable::Unavailable {
                            reason: SymbolUnavailableReason::InsufficientCoverage,
                            view: None,
                            warnings: selected.warnings,
                        }),
                        &budget,
                        deadline,
                    )
                    .await;
            }
            self.capture_query_fact_response(selected, request_maximum, requests, &budget, deadline)
                .await
        })
        .await
    }

    fn query_fact_view(
        &self,
        view: Option<CapturedViewId>,
    ) -> Result<Result<SelectedFactView, SearchUnavailable>, ToolFailure> {
        let Some(view) = view else {
            return Ok(Ok(None));
        };
        let selected = self.captured_context(&view, "query facts")?;
        Ok(match self.query_selection(&selected.view)? {
            Ok(Some(material)) => Ok(Some((material, selected.view))),
            Ok(None) => Err(unavailable(SymbolUnavailableReason::ViewExpired)),
            Err(reason) => Err(unavailable(reason)),
        })
    }

    async fn decode_query_fact_request(
        &self,
        request: rmcp::model::CallToolRequestParams,
        budget: &QueryBudget,
        deadline: super::super::RequestDeadline,
    ) -> Result<QueryFactRequest, ToolFailure> {
        self.query_work(budget, deadline, move |budget| {
            let arguments = request.arguments.unwrap_or_default();
            budget.charge(&arguments)?;
            let request: QueryExchange<serde_json::Value> =
                serde_json::from_value(arguments.into()).map_err(|_| {
                    errors::server::read_invalid()
                        .field("arguments")
                        .violation("query fact arguments do not match the documented form")
                        .error()
                })?;
            let request = serde_json::from_value::<QueryFactRequest>(request.into_payload()?)
                .map_err(|_| {
                    errors::server::read_invalid()
                        .field("arguments")
                        .violation("query fact arguments do not match the documented form")
                        .error()
                })?;
            request.requirements.validate_selection()?;
            Ok(request)
        })
        .await
    }

    async fn capture_query_fact_response(
        &self,
        selected: super::QueryAcquisition,
        request_maximum: u64,
        requests: rift_server::ProviderRequests,
        budget: &QueryBudget,
        deadline: super::super::RequestDeadline,
    ) -> Result<serde_json::Value, ToolFailure> {
        let views = std::sync::Arc::clone(&self.local_views);
        let responses = std::sync::Arc::clone(&self.candidate_responses);
        self.query_work(budget, deadline, move |budget| {
            let views = views.get().ok_or_else(retention_failure)?;
            let root = selected.material.root().to_path_buf();
            let root_text = root.to_str().ok_or_else(retention_failure)?.to_owned();
            let owner = selected.material.owner().clone();
            let selection = selected
                .material
                .captured_provider_selection()
                .ok_or_else(|| acquisition_failure("captured provider selection is unavailable"))?;
            budget.charge(selection)?;
            let selection = selection.clone();
            let mut response = None;
            let admitted = views.capture_with(selected.material, |view| {
                let value =
                    QueryExchange::new(QueryFactResult::Facts(Box::new(QueryFactSelection {
                        root: root_text,
                        owner: owner.clone(),
                        view: view.clone(),
                        selection,
                        requests: request_maximum.saturating_sub(requests.remaining()),
                        facts: selected.facts,
                        services: selected.services,
                        warnings: selected.warnings,
                        upstream: selected.upstream,
                    })));
                budget.charge(&value)?;
                let value = serde_json::to_value(value).map_err(encoding_failure)?;
                budget.check()?;
                if !responses.save_symbol(&root, &owner, view)? {
                    return Ok(false);
                }
                response = Some(value);
                Ok(true)
            })?;
            if admitted.is_none() {
                return Err(retention_failure());
            }
            response.ok_or_else(retention_failure)
        })
        .await
    }

    async fn encode_query_fact_response(
        &self,
        response: QueryFactResult,
        budget: &QueryBudget,
        deadline: super::super::RequestDeadline,
    ) -> Result<serde_json::Value, ToolFailure> {
        self.query_work(budget, deadline, move |budget| {
            let response = QueryExchange::new(response);
            budget.charge(&response)?;
            let response = serde_json::to_value(response).map_err(encoding_failure)?;
            budget.check()?;
            Ok(response)
        })
        .await
    }
}

fn unavailable(reason: SymbolUnavailableReason) -> SearchUnavailable {
    SearchUnavailable::Unavailable {
        reason,
        view: None,
        warnings: Vec::new(),
    }
}

fn retention_failure() -> RiftError {
    acquisition_failure("query fact material could not be retained within its bound")
}

fn encoding_failure(source: serde_json::Error) -> RiftError {
    errors::mcp::forward_failed().source(source).error()
}
