//! Exact source results are acquired through the original read owner before evaluation.

use rift_protocol::source_read::{GetSourceParams, GetSourceResult};
use rift_protocol::symbol_read::{CapturedView, SymbolUnavailableReason};
use rift_search::sparql::FactRequirements;

use super::{
    Json, QueryExecution, RiftMcp, SearchUnavailable, SelectedQueryServices, ToolFailure,
    acquisition_failure, failure, selection,
};

impl RiftMcp {
    pub(super) async fn include_query_sources(
        &self,
        mut selected: SelectedQueryServices,
        requirements: &FactRequirements,
        selected_view: Option<&CapturedView>,
        execution: QueryExecution<'_>,
    ) -> Result<Result<SelectedQueryServices, SearchUnavailable>, ToolFailure> {
        let global_configuration = self
            .published
            .read()
            .await
            .current
            .configuration
            .global_configuration();
        let mut local_view = selected_view.cloned();
        for request in &requirements.source_reads {
            let (params, expected) = match Self::query_source_parameters(
                &selected,
                request,
                selected_view,
                local_view.as_ref(),
                execution,
            )? {
                Ok(selected) => selected,
                Err(SearchUnavailable::Unavailable { reason, view, .. }) => {
                    return Ok(Err(SearchUnavailable::Unavailable {
                        reason,
                        view,
                        warnings: selected.warnings,
                    }));
                }
            };
            let context = std::sync::Mutex::new(expected);
            let context = &context;
            if let Some(reason) = self.source_request_context(&params, context)? {
                return Ok(Err(SearchUnavailable::Unavailable {
                    reason,
                    view: execution
                        .source_context
                        .map(|context| context.view.clone())
                        .or_else(|| request.view.as_ref().and_then(|_| local_view.clone())),
                    warnings: selected.warnings,
                }));
            }
            let result = self
                .acquire_source_result(params, context, execution)
                .await?;
            let (view, warnings) = match complete_source_result(&result) {
                Ok(selected) => selected,
                Err(reason) => return Ok(Err(reason)),
            };
            if !result.is_valid_for(request) {
                return Err(failure(acquisition_failure(
                    "source result differs from its original request",
                )));
            }
            selected.include_view(view)?;
            if request.unit.is_project() && request.rev.is_none() && local_view.is_none() {
                local_view = Some(view.clone());
            }
            selected.warnings.extend(warnings.iter().cloned());
            selected = match self
                .include_source_material(selected, request, view, execution)
                .await?
            {
                Ok(selected) => selected,
                Err(reason) => return Ok(Err(reason)),
            };
            if !source_selection_is_recorded(&selected.upstream, &request.unit).map_err(failure)?
                && let Some(upstream) =
                    selection::source_selection(&request.unit, view, &global_configuration)
                        .map_err(failure)?
                && let Err(reason) = selection::include(&mut selected.upstream, upstream)
            {
                return Ok(Err(SearchUnavailable::Unavailable {
                    reason,
                    view: Some(view.clone()),
                    warnings: selected.warnings,
                }));
            }
            append_query_source(&mut selected, request, result, execution.budget)?;
        }
        if self
            .published
            .read()
            .await
            .current
            .configuration
            .global_configuration()
            != global_configuration
        {
            return Err(failure(acquisition_failure(
                "global configuration changed during query acquisition",
            )));
        }
        Ok(Ok(selected))
    }
    async fn acquire_source_result(
        &self,
        params: GetSourceParams,
        context: &std::sync::Mutex<Option<crate::global::candidates::SavedCandidateBackend>>,
        execution: QueryExecution<'_>,
    ) -> Result<GetSourceResult, ToolFailure> {
        execution.requests.claim().map_err(failure)?;
        let Json(result) = tokio::time::timeout_at(
            execution.deadline.at(),
            self.exact_source_with_context(params, Some(context)),
        )
        .await
        .map_err(|_| {
            failure(acquisition_failure(
                "source acquisition exceeded the request deadline",
            ))
        })??;
        self.admit_query_data(execution.budget, result, execution.deadline)
            .await
    }

    fn query_source_parameters(
        selected: &SelectedQueryServices,
        request: &GetSourceParams,
        selected_view: Option<&CapturedView>,
        local_view: Option<&CapturedView>,
        execution: QueryExecution<'_>,
    ) -> Result<Result<SourceParameters, SearchUnavailable>, ToolFailure> {
        let mut admission = execution.budget.clone();
        admission.charge(request).map_err(failure)?;
        let mut params = request.clone();
        if params.view.is_none()
            && params.rev.is_none()
            && params.unit.is_project()
            && let Some(view) = local_view
        {
            params.view = Some(view.id.clone());
        }
        if let Some(view) = selected_view {
            if params
                .view
                .as_ref()
                .is_some_and(|requested| requested != &view.id)
                || params.rev.is_some()
            {
                return Ok(Err(SearchUnavailable::Unavailable {
                    reason: SymbolUnavailableReason::ViewContext,
                    view: Some(view.clone()),
                    warnings: Vec::new(),
                }));
            }
            params.view = Some(view.id.clone());
        }
        if let Some(context) = execution.source_context {
            admission.charge(context).map_err(failure)?;
        }
        let mut expected = execution.source_context.cloned();
        if params.view.is_none() && params.rev.is_none() {
            let (owner, _) =
                rift_protocol::identity::parse_source_unit_identity(params.unit.as_str())
                    .map_err(failure)?;
            if let Some(owner) = owner {
                let mut matching = selected
                    .upstream
                    .iter()
                    .filter(|selected| selected.owners.contains(&owner));
                if let Some(upstream) = matching.next() {
                    if matching.next().is_some() {
                        return Ok(Err(SearchUnavailable::Unavailable {
                            reason: SymbolUnavailableReason::ViewContext,
                            view: None,
                            warnings: Vec::new(),
                        }));
                    }
                    params.view = Some(upstream.view.id.clone());
                    expected = Some(crate::global::candidates::SavedCandidateBackend {
                        backend: "global".to_owned(),
                        root: upstream.configuration.endpoint.clone(),
                        owner: None,
                        view: upstream.view.clone(),
                        project: None,
                        upstream_view: None,
                    });
                }
            }
        }
        Ok(Ok((params, expected)))
    }

    async fn include_source_material(
        &self,
        mut selected: SelectedQueryServices,
        request: &GetSourceParams,
        view: &CapturedView,
        execution: QueryExecution<'_>,
    ) -> Result<Result<SelectedQueryServices, SearchUnavailable>, ToolFailure> {
        let provider = self
            .source_material_provider(request, view, execution)
            .await?;
        if provider
            .as_ref()
            .is_some_and(|provider| provider.backend != "local")
        {
            return Ok(Err(SearchUnavailable::Unavailable {
                reason: SymbolUnavailableReason::ViewContext,
                view: Some(view.clone()),
                warnings: selected.warnings,
            }));
        }
        let actor = provider.as_ref().map_or(view, |provider| &provider.view);
        match self.query_selection(actor)? {
            Ok(Some(material)) => {
                if let Some(provider) = &provider {
                    if material.root().to_str() != Some(provider.root.as_str())
                        || material.owner() != &provider.owner
                    {
                        return Ok(Err(SearchUnavailable::Unavailable {
                            reason: SymbolUnavailableReason::ViewContext,
                            view: Some(view.clone()),
                            warnings: selected.warnings,
                        }));
                    }
                    material
                        .validate_provider_selection(&provider.selection)
                        .map_err(failure)?;
                }
                let material = self
                    .source_provider_selection(
                        material,
                        provider.as_ref().map(|provider| &provider.selection),
                        execution,
                    )
                    .await?;
                let (updated, admitted) = self
                    .include_query_material(
                        selected,
                        material,
                        execution.budget,
                        execution.deadline,
                    )
                    .await?;
                selected = updated;
                if let Err(reason) = admitted {
                    return Ok(Err(SearchUnavailable::Unavailable {
                        reason,
                        view: Some(view.clone()),
                        warnings: selected.warnings,
                    }));
                }
            }
            Ok(None) if provider.is_some() => {
                return Ok(Err(SearchUnavailable::Unavailable {
                    reason: SymbolUnavailableReason::ViewContext,
                    view: Some(view.clone()),
                    warnings: selected.warnings,
                }));
            }
            Ok(None) => {}
            Err(reason) => {
                return Ok(Err(SearchUnavailable::Unavailable {
                    reason,
                    view: Some(view.clone()),
                    warnings: selected.warnings,
                }));
            }
        }
        if let Some(provider) = provider {
            selected = self
                .include_source_upstream(selected, provider.upstream, execution)
                .await?;
        }
        Ok(Ok(selected))
    }

    async fn source_material_provider(
        &self,
        request: &GetSourceParams,
        view: &CapturedView,
        execution: QueryExecution<'_>,
    ) -> Result<Option<crate::proxy::query_material::Provider>, ToolFailure> {
        let Some(captured) = self.captured_query_material(&view.id, "source")? else {
            return Ok(None);
        };
        let mut admission = execution.budget.clone();
        admission.charge(&request.unit).map_err(failure)?;
        let unit = request.unit.clone();
        self.query_work(execution.budget, execution.deadline, move |budget| {
            budget.charge_bytes(captured.material().metadata())?;
            let metadata = crate::proxy::query_material::metadata(&captured)?;
            let source = crate::proxy::query_material_read::selected_source(&metadata, &unit)?;
            let provider = source.provider;
            metadata
                .providers
                .into_iter()
                .nth(provider)
                .map(Some)
                .ok_or_else(|| acquisition_failure("captured provider selection is unavailable"))
        })
        .await
    }

    async fn source_provider_selection(
        &self,
        material: rift_server::LocalSymbolViewInput,
        recorded: Option<&rift_search::query::QueryProviderSelection>,
        execution: QueryExecution<'_>,
    ) -> Result<rift_server::LocalSymbolViewInput, ToolFailure> {
        let configuration = self
            .published
            .read()
            .await
            .current
            .configuration
            .accepted_for_tool(super::wire::ErrorPhase::Read)?;
        let mut admission = execution.budget.clone();
        admission.charge(&recorded).map_err(failure)?;
        let recorded = recorded.cloned();
        self.query_work(execution.budget, execution.deadline, move |budget| {
            budget.charge(&configuration)?;
            let selection = if let Some(recorded) = recorded {
                material.validate_provider_selection(&recorded)?;
                if material
                    .captured_provider_selection()
                    .is_some_and(|selected| selected != &recorded)
                {
                    return Err(acquisition_failure(
                        "captured provider selection differs from its original publication",
                    ));
                }
                recorded
            } else if let Some(selected) = material.captured_provider_selection() {
                material.validate_provider_selection(selected)?;
                budget.charge(selected)?;
                selected.clone()
            } else {
                let selection = material.provider_selection(&configuration)?;
                budget.charge(&selection)?;
                selection
            };
            if selection.configuration
                != rift_search::query::QueryProviderSelection::configuration_digest(&configuration)?
            {
                return Err(acquisition_failure(
                    "captured provider configuration does not match",
                ));
            }
            material.with_provider_selection(selection)
        })
        .await
    }

    async fn include_source_upstream(
        &self,
        mut selected: SelectedQueryServices,
        upstream: Vec<crate::server::candidates::QueryGlobalSelection>,
        execution: QueryExecution<'_>,
    ) -> Result<SelectedQueryServices, ToolFailure> {
        self.query_work(execution.budget, execution.deadline, move |budget| {
            if selected.upstream.is_empty() {
                selected.upstream = upstream;
                return Ok(selected);
            }
            budget.charge(&upstream)?;
            budget.charge_material(
                upstream
                    .len()
                    .checked_mul(size_of::<crate::server::candidates::QueryGlobalSelection>())
                    .ok_or_else(|| acquisition_failure("query service admission failed"))?,
            )?;
            selected
                .upstream
                .try_reserve_exact(upstream.len())
                .map_err(|_| acquisition_failure("query service admission failed"))?;
            for source in upstream {
                budget.check()?;
                selection::include(&mut selected.upstream, source).map_err(|_| {
                    acquisition_failure(
                        "captured provider selection differs from its original publication",
                    )
                })?;
            }
            Ok(selected)
        })
        .await
    }

    fn source_request_context(
        &self,
        request: &GetSourceParams,
        context: &std::sync::Mutex<Option<crate::global::candidates::SavedCandidateBackend>>,
    ) -> Result<Option<SymbolUnavailableReason>, ToolFailure> {
        let context = context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(context) = context.as_ref() else {
            if let Some(view) = &request.view
                && !request.unit.is_project()
                && self.captured_query_material(view, "source")?.is_none()
            {
                return Ok(Some(SymbolUnavailableReason::ViewContext));
            }
            return Ok(None);
        };
        if request.view.as_ref() != Some(&context.view.id) {
            return Ok(Some(SymbolUnavailableReason::ViewContext));
        }
        if crate::proxy::captured_expiry(&context.view).map_err(failure)?
            <= std::time::SystemTime::now()
        {
            return Ok(Some(SymbolUnavailableReason::ViewExpired));
        }
        if context.backend == "global" {
            let (owner, _) =
                rift_protocol::identity::parse_source_unit_identity(request.unit.as_str())
                    .map_err(failure)?;
            if !matches!(
                owner,
                Some(
                    rift_protocol::identity::SymbolOwner::Package { .. }
                        | rift_protocol::identity::SymbolOwner::Runtime { .. }
                )
            ) {
                return Ok(Some(SymbolUnavailableReason::ViewContext));
            }
        } else {
            let original = self.captured_context(&context.view.id, "source")?;
            if original.root != context.root || context.owner.as_ref() != Some(&original.owner) {
                return Ok(Some(SymbolUnavailableReason::ViewContext));
            }
        }
        Ok(None)
    }
}

type SourceParameters = (
    GetSourceParams,
    Option<crate::global::candidates::SavedCandidateBackend>,
);
type SourceWitness<'a> = (&'a CapturedView, &'a [rift_protocol::read::ReadWarning]);

fn complete_source_result(
    result: &GetSourceResult,
) -> Result<SourceWitness<'_>, SearchUnavailable> {
    Ok(match result {
        GetSourceResult::Found {
            source_complete: true,
            view,
            warnings,
            ..
        }
        | GetSourceResult::Missing { view, warnings, .. } => (view, warnings.as_slice()),
        GetSourceResult::Found { view, warnings, .. } => {
            return Err(SearchUnavailable::Unavailable {
                reason: SymbolUnavailableReason::InsufficientCoverage,
                view: Some(view.clone()),
                warnings: warnings.clone(),
            });
        }
        GetSourceResult::Unavailable {
            view,
            reason,
            warnings,
            ..
        } => {
            return Err(SearchUnavailable::Unavailable {
                reason: *reason,
                view: view.clone(),
                warnings: warnings.clone(),
            });
        }
    })
}

fn append_query_source(
    selected: &mut SelectedQueryServices,
    request: &GetSourceParams,
    result: GetSourceResult,
    budget: &super::QueryBudget,
) -> Result<(), ToolFailure> {
    let mut admission = budget.clone();
    admission.charge(request).map_err(failure)?;
    admission
        .charge_material(size_of::<(GetSourceParams, GetSourceResult)>())
        .map_err(failure)?;
    selected
        .services
        .sources
        .try_reserve_exact(1)
        .map_err(|_| failure(acquisition_failure("source result admission failed")))?;
    selected.services.sources.push((request.clone(), result));
    Ok(())
}

fn source_selection_is_recorded(
    selected: &[crate::server::candidates::QueryGlobalSelection],
    unit: &rift_protocol::read::SourceUnitId,
) -> Result<bool, rift_error::RiftError> {
    let (owner, _) = rift_protocol::identity::parse_source_unit_identity(unit.as_str())?;
    Ok(owner.as_ref().is_some_and(|owner| {
        selected
            .iter()
            .any(|selection| selection.owners.contains(owner))
    }))
}
