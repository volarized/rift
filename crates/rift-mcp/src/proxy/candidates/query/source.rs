//! Exact reads use admitted query offsets or their original backend selection.

use rift_error::{RiftError, errors};
use rift_protocol::source_read::{GetSourceParams, GetSourceResult};
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};
use rift_search::sparql::{PageForm, PageSelection, QueryLimits};
use rmcp::handler::server::tool::IntoCallToolResult as _;
use rmcp::model::{CallToolRequestParams, CallToolResponse};

use super::super::RiftProxy;
use super::retained::{self, Metadata};

impl RiftProxy {
    pub(super) async fn query_material_page(
        &self,
        selection: PageSelection,
        deadline: tokio::time::Instant,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let Some(views) = self.query_views.get() else {
            return Ok(None);
        };
        let root = self
            .accepted_root
            .as_deref()
            .ok_or_else(retained::unavailable)?;
        let id = CapturedViewId::parse(&selection.view)?;
        let captured = match views.query_view(&id, root, &self.owner) {
            Ok(captured) => captured,
            Err(SymbolUnavailableReason::ViewContext) => return Ok(None),
            Err(_) => return Err(retained::unavailable()),
        };
        let limits = QueryLimits::try_from(&self.query_configuration)?;
        let budget =
            rift_server::QueryFactBudget::for_request(limits, deadline.into_std(), &|| false)?;
        let backends = self.query_roots();
        self.query_work(&budget, deadline, move |budget| {
            budget.charge_bytes(captured.material().metadata())?;
            let saved = retained::metadata(&captured)?;
            validate_backends(&saved, &backends)?;
            let answer = saved
                .answer
                .decode_bytes(captured.material().records(), Some(selection.page))?;
            match (&selection.form, &answer) {
                (
                    PageForm::Bindings {
                        variables: selected,
                    },
                    rift_protocol::query::QueryAnswer::Bindings { variables, .. },
                ) if selected == variables => {}
                (PageForm::Graph, rift_protocol::query::QueryAnswer::Graph { .. }) => {}
                _ => return Err(retained::unavailable()),
            }
            let response = saved.answer.response(answer, selection.page)?;
            budget.charge(&response)?;
            Ok(Some(crate::output::Json(response).into_call_tool_result()))
        })
        .await
    }

    pub(in crate::proxy) async fn query_material_read(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        if request.name != "get_symbol" {
            return Ok(None);
        }
        let arguments = request.arguments.clone().unwrap_or_default();
        let Some(view) = arguments.get("view").and_then(serde_json::Value::as_str) else {
            return Ok(None);
        };
        let view = CapturedViewId::parse(view)?;
        let Some(views) = self.query_views.get() else {
            return Ok(None);
        };
        let root = self
            .accepted_root
            .as_deref()
            .ok_or_else(retained::unavailable)?;
        let captured = match views.query_view(&view, root, &self.owner) {
            Ok(captured) => captured,
            Err(SymbolUnavailableReason::ViewContext) => return Ok(None),
            Err(_) => return Err(retained::unavailable()),
        };
        let limits = QueryLimits::try_from(&self.query_configuration)?;
        let deadline = tokio::time::Instant::now() + self.forward_budget;
        let budget =
            rift_server::QueryFactBudget::for_request(limits, deadline.into_std(), &|| false)?;
        let backends = self.query_roots();
        let request = request.clone();
        let selected = self
            .query_work(&budget, deadline, move |budget| {
                budget.charge_bytes(captured.material().metadata())?;
                let saved = retained::metadata(&captured)?;
                validate_backends(&saved, &backends)?;
                select_read(request, &captured, &saved, budget)
            })
            .await?;
        let SelectedRead {
            backend,
            request,
            view,
            source_units,
            original_view,
        } = selected;
        let response = self
            .query_backend(&backend)?
            .forward_read((*request).clone())
            .await?;
        self.query_work(&budget, deadline, move |budget| {
            if let Ok(CallToolResponse::Complete(response)) = &response {
                budget.charge(response)?;
            }
            let response = qualify_response(
                response,
                &request,
                &backend,
                view,
                &source_units,
                &original_view,
            )?;
            if let Ok(CallToolResponse::Complete(response)) = &response {
                budget.charge(response)?;
            }
            Ok(Some(response))
        })
        .await
    }

    pub(in crate::proxy) fn query_roots(&self) -> std::collections::BTreeMap<String, String> {
        std::iter::once(("local".to_owned(), self))
            .chain(
                self.backends
                    .iter()
                    .map(|(name, proxy)| (name.clone(), proxy)),
            )
            .filter_map(|(name, proxy)| {
                proxy
                    .accepted_root
                    .as_deref()
                    .and_then(std::path::Path::to_str)
                    .map(|root| (name, root.to_owned()))
            })
            .collect()
    }
}

pub(super) fn validate_backends(
    saved: &Metadata,
    backends: &std::collections::BTreeMap<String, String>,
) -> Result<(), RiftError> {
    for provider in &saved.providers {
        if backends.get(&provider.backend) != Some(&provider.root) {
            return Err(retained::unavailable());
        }
    }
    Ok(())
}

fn encoding_failure(source: serde_json::Error) -> RiftError {
    errors::mcp::forward_failed().source(source).error()
}

struct SelectedRead {
    backend: String,
    request: Box<CallToolRequestParams>,
    view: CapturedView,
    source_units: Vec<(
        rift_protocol::read::SourceUnitId,
        rift_protocol::read::SourceUnitId,
    )>,
    original_view: CapturedView,
}

fn select_read(
    request: CallToolRequestParams,
    captured: &rift_server::CapturedQueryView,
    saved: &Metadata,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<SelectedRead, RiftError> {
    let mut arguments = request.arguments.unwrap_or_default();
    let id = arguments
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(retained::unavailable)?;
    let (provider, original) = selected_symbol(saved, id)?;
    arguments.insert(
        "id".to_owned(),
        serde_json::to_value(original).map_err(encoding_failure)?,
    );
    let provider_index = provider;
    let provider = saved
        .providers
        .get(provider)
        .ok_or_else(retained::unavailable)?;
    let source_units = saved
        .sources
        .iter()
        .filter(|source| source.provider == provider_index)
        .map(|source| (source.original.clone(), source.public.clone()))
        .collect();
    budget.charge(&provider.view)?;
    let original_view = provider.view.clone();
    arguments.insert(
        "view".to_owned(),
        serde_json::to_value(&provider.view.id).map_err(encoding_failure)?,
    );
    Ok(SelectedRead {
        backend: provider.backend.clone(),
        request: Box::new(CallToolRequestParams::new(request.name).with_arguments(arguments)),
        view: captured.view().clone(),
        source_units,
        original_view,
    })
}

pub(crate) fn selected_source<'a>(
    saved: &'a Metadata,
    unit: &rift_protocol::read::SourceUnitId,
) -> Result<&'a super::retained::Source, RiftError> {
    let mut matches = saved.sources.iter().filter(|source| source.public == *unit);
    let source = matches.next().ok_or_else(retained::unavailable)?;
    if matches.next().is_some() {
        return Err(retained::unavailable());
    }
    Ok(source)
}

pub(crate) fn selected_symbol<'a>(
    saved: &'a Metadata,
    id: &str,
) -> Result<(usize, &'a rift_protocol::read::SymbolId), RiftError> {
    let mut matches = saved
        .providers
        .iter()
        .enumerate()
        .filter_map(|(index, provider)| {
            provider
                .symbols
                .iter()
                .find(|(_, public)| public.as_str() == id)
                .map(|(original, _)| (index, original))
        });
    let selected = matches.next().ok_or_else(retained::unavailable)?;
    if matches.next().is_some() {
        return Err(retained::unavailable());
    }
    Ok(selected)
}

pub(crate) fn captured_source(
    params: GetSourceParams,
    bytes: &[u8],
    view: &CapturedView,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<GetSourceResult, RiftError> {
    let text = std::str::from_utf8(bytes).map_err(|_| retained::unavailable())?;
    let range = params.range.unwrap_or(rift_protocol::read::TextRange {
        start: 0,
        end: u64::try_from(text.len()).map_err(|_| retained::unavailable())?,
    });
    let start = usize::try_from(range.start).map_err(|_| retained::unavailable())?;
    let end = usize::try_from(range.end).map_err(|_| retained::unavailable())?;
    let excerpt = text.get(start..end).ok_or_else(|| {
        errors::server::read_invalid()
            .field("range")
            .violation("source range must contain complete UTF-8 bytes")
            .error()
    })?;
    budget.charge_bytes(excerpt.as_bytes())?;
    let response = GetSourceResult::Found {
        source: rift_protocol::read::SourceExcerpt {
            span: rift_protocol::read::SourceUnitSpan {
                unit: params.unit,
                range,
            },
            text: excerpt.to_owned(),
        },
        source_complete: true,
        view: view.clone(),
        warnings: Vec::new(),
    };
    budget.charge(&response)?;
    Ok(response)
}

fn qualify_response(
    response: Result<CallToolResponse, rmcp::ErrorData>,
    request: &CallToolRequestParams,
    backend: &str,
    view: CapturedView,
    source_units: &[(
        rift_protocol::read::SourceUnitId,
        rift_protocol::read::SourceUnitId,
    )],
    original_view: &CapturedView,
) -> Result<Result<CallToolResponse, rmcp::ErrorData>, RiftError> {
    let Ok(CallToolResponse::Complete(mut result)) = response else {
        return Ok(response);
    };
    if !crate::proxy::candidates::read::canonical_symbol_response(&result) {
        return Ok(Ok(result.into()));
    }
    let value = result
        .structured_content
        .take()
        .ok_or_else(retained::unavailable)?;
    let mut symbol: rift_protocol::symbol_read::GetSymbolResult =
        serde_json::from_value(value).map_err(encoding_failure)?;
    let original: rift_protocol::symbol_read::GetSymbolParams =
        serde_json::from_value(request.arguments.clone().unwrap_or_default().into())
            .map_err(encoding_failure)?;
    crate::proxy::candidates::read::validate_symbol(&original, &symbol, Some(original_view))?;
    symbol = qualify_symbol(symbol, &original, backend, view, source_units)?;
    Ok(crate::output::Json(symbol).into_call_tool_result())
}

pub(crate) fn qualify_source(
    mut source: GetSourceResult,
    params: &GetSourceParams,
    backend: &str,
    view: CapturedView,
    public_unit: Option<rift_protocol::read::SourceUnitId>,
) -> Result<GetSourceResult, RiftError> {
    if !source.is_valid_for(params) {
        return Err(retained::unavailable());
    }
    let unit = public_unit.ok_or_else(retained::unavailable)?;
    match &mut source {
        GetSourceResult::Found {
            source,
            view: selected,
            ..
        } => {
            source.span.unit = unit;
            *selected = view;
        }
        GetSourceResult::Missing {
            unit: selected,
            view: selected_view,
            ..
        } => {
            *selected = unit;
            *selected_view = view;
        }
        GetSourceResult::Unavailable {
            unit: selected,
            view: selected_view,
            ..
        } => {
            *selected = unit;
            *selected_view = Some(view);
        }
    }
    crate::proxy::identity::qualify_source(&mut source, backend)?;
    Ok(source)
}

pub(crate) fn qualify_symbol(
    mut symbol: rift_protocol::symbol_read::GetSymbolResult,
    original: &rift_protocol::symbol_read::GetSymbolParams,
    backend: &str,
    view: CapturedView,
    source_units: &[(
        rift_protocol::read::SourceUnitId,
        rift_protocol::read::SourceUnitId,
    )],
) -> Result<rift_protocol::symbol_read::GetSymbolResult, RiftError> {
    if !symbol.is_valid_for(&original.id) {
        return Err(retained::unavailable());
    }
    crate::proxy::identity::qualify_symbol_result(&mut symbol, backend)?;
    if let rift_protocol::symbol_read::GetSymbolResult::Found { declarations, .. } = &mut symbol {
        for declaration in declarations {
            qualify_declaration_unit(declaration, backend, source_units)?;
        }
    }
    match &mut symbol {
        rift_protocol::symbol_read::GetSymbolResult::Found { view: selected, .. }
        | rift_protocol::symbol_read::GetSymbolResult::Missing { view: selected, .. } => {
            *selected = view;
        }
        rift_protocol::symbol_read::GetSymbolResult::Unavailable { view: selected, .. } => {
            *selected = Some(view);
        }
    }
    Ok(symbol)
}

fn qualify_declaration_unit(
    declaration: &mut rift_protocol::symbol_read::SymbolDeclaration,
    backend: &str,
    source_units: &[(
        rift_protocol::read::SourceUnitId,
        rift_protocol::read::SourceUnitId,
    )],
) -> Result<(), RiftError> {
    let project_unit = match (&declaration.unit, &declaration.path) {
        (None, Some(path)) => {
            let unit = rift_core::SourceUnitId::new(
                rift_core::SourceResolverId::new("project")?,
                rift_core::SourcePath::new(path.0.as_str())?,
            )?;
            Some(rift_protocol::read::SourceUnitId::parse(&unit.to_string())?)
        }
        _ => None,
    };
    let original = declaration
        .unit
        .as_ref()
        .or(project_unit.as_ref())
        .ok_or_else(retained::unavailable)?;
    let mut matches = source_units.iter().filter(|(unit, _)| unit == original);
    if let Some((_, public)) = matches.next() {
        if matches.next().is_some() {
            return Err(retained::unavailable());
        }
        if public != original {
            declaration.unit = Some(public.clone());
            declaration.path = None;
        }
    } else if (backend != "local"
        || source_units
            .iter()
            .any(|(original, public)| original != public))
        && rift_protocol::identity::parse_source_unit_identity(original.as_str())?
            .0
            .is_none()
    {
        return Err(retained::unavailable());
    }
    Ok(())
}

struct SourceReadSelection {
    backend: String,
    request: GetSourceParams,
    view: Option<CapturedViewId>,
    context: Option<crate::global::candidates::SavedCandidateBackend>,
    public_view: Option<CapturedView>,
}

impl RiftProxy {
    pub(super) async fn source_query_input(
        &self,
        request: &GetSourceParams,
        budget: &rift_server::QueryFactBudget<'static>,
        requests: &rift_server::ProviderRequests,
        deadline: tokio::time::Instant,
    ) -> Result<super::acquire::Acquired, RiftError> {
        if !request.is_valid() {
            return Err(retained::unavailable());
        }
        if let Some(captured) = self
            .captured_source_query_input(request, budget, deadline)
            .await?
        {
            return Ok(captured);
        }
        let selected = self.select_source_read(request, budget, deadline).await?;
        let original = selected.request.clone();
        let requirements = rift_search::sparql::FactRequirements {
            services: vec![rift_search::sparql::SOURCE_SERVICE.to_owned()],
            source_reads: vec![selected.request],
            ..rift_search::sparql::FactRequirements::default()
        };
        let acquired = self
            .query_input(
                &selected.backend,
                requirements,
                (selected.view, selected.context),
                budget,
                requests,
                deadline,
            )
            .await?;
        let super::acquire::Acquired::Facts(facts) = acquired else {
            let public_view = selected.public_view;
            let original_view = original.view;
            return self
                .query_work(budget, deadline, move |budget| {
                    qualify_source_unavailable(
                        acquired,
                        original_view.as_ref(),
                        public_view,
                        budget,
                    )
                })
                .await;
        };
        let public = request.clone();
        let backend = selected.backend;
        let public_view = selected.public_view;
        let retained_views = std::sync::Arc::clone(&self.candidate_views);
        self.query_work(budget, deadline, move |budget| {
            qualify_source_facts(
                facts,
                &original,
                &public,
                &backend,
                public_view.as_ref(),
                &retained_views,
                budget,
            )
        })
        .await
    }

    async fn captured_source_query_input(
        &self,
        request: &GetSourceParams,
        budget: &rift_server::QueryFactBudget<'static>,
        deadline: tokio::time::Instant,
    ) -> Result<Option<super::acquire::Acquired>, RiftError> {
        let (Some(view), Some(views)) = (&request.view, self.query_views.get()) else {
            return Ok(None);
        };
        let root = self
            .accepted_root
            .as_deref()
            .ok_or_else(retained::unavailable)?;
        let captured = match views.query_view(view, root, &self.owner) {
            Ok(captured) => captured,
            Err(SymbolUnavailableReason::ViewContext) => return Ok(None),
            Err(_) => return Err(retained::unavailable()),
        };
        let backends = self.query_roots();
        let request = request.clone();
        self.query_work(budget, deadline, move |budget| {
            budget.charge_bytes(captured.material().metadata())?;
            let saved = retained::metadata(&captured)?;
            validate_backends(&saved, &backends)?;
            let source = selected_source(&saved, &request.unit)?;
            let Some(range) = &source.bytes else {
                return Ok(None);
            };
            let bytes = captured
                .material()
                .sources()
                .get(range.clone())
                .ok_or_else(retained::unavailable)?;
            let provider = saved
                .providers
                .get(source.provider)
                .ok_or_else(retained::unavailable)?;
            budget.charge(&request)?;
            let original = GetSourceParams {
                unit: source.original.clone(),
                view: Some(provider.view.id.clone()),
                ..request.clone()
            };
            let result = captured_source(original.clone(), bytes, &provider.view, budget)?;
            budget.charge(&result)?;
            let public = qualify_source(
                result.clone(),
                &original,
                &provider.backend,
                captured.view().clone(),
                Some(request.unit.clone()),
            )?;
            if !public.is_valid_for(&request) {
                return Err(retained::unavailable());
            }
            Ok(Some(captured_source_input(
                provider, source, request, original, result, public, budget,
            )?))
        })
        .await
    }

    async fn select_source_read(
        &self,
        request: &GetSourceParams,
        budget: &rift_server::QueryFactBudget<'static>,
        deadline: tokio::time::Instant,
    ) -> Result<SourceReadSelection, RiftError> {
        let Some(view) = &request.view else {
            return unheld_source_read(request);
        };
        if let Some(views) = self.query_views.get() {
            let root = self
                .accepted_root
                .as_deref()
                .ok_or_else(retained::unavailable)?;
            match views.query_view(view, root, &self.owner) {
                Ok(captured) => {
                    let backends = self.query_roots();
                    let request = request.clone();
                    return self
                        .query_work(budget, deadline, move |budget| {
                            budget.charge_bytes(captured.material().metadata())?;
                            let saved = retained::metadata(&captured)?;
                            validate_backends(&saved, &backends)?;
                            let source = selected_source(&saved, &request.unit)?;
                            let provider = saved
                                .providers
                                .get(source.provider)
                                .ok_or_else(retained::unavailable)?;
                            let mut original = request;
                            original.unit = source.original.clone();
                            original.view = Some(provider.view.id.clone());
                            original.rev = None;
                            let mut context = crate::global::candidates::SavedCandidateBackend {
                                backend: provider.backend.clone(),
                                root: provider.root.clone(),
                                owner: Some(provider.owner.clone()),
                                view: provider.view.clone(),
                                project: None,
                                upstream_view: None,
                            };
                            let mut view = Some(provider.view.id.clone());
                            if let (Some(owner), _) =
                                rift_protocol::identity::parse_source_unit_identity(
                                    original.unit.as_str(),
                                )?
                            {
                                let mut matching = provider
                                    .upstream
                                    .iter()
                                    .filter(|selected| selected.owners.contains(&owner));
                                let upstream = matching.next().ok_or_else(retained::unavailable)?;
                                if matching.next().is_some() {
                                    return Err(retained::unavailable());
                                }
                                original.view = Some(upstream.view.id.clone());
                                context = crate::global::candidates::SavedCandidateBackend {
                                    backend: "global".to_owned(),
                                    root: upstream.configuration.endpoint.clone(),
                                    owner: None,
                                    view: upstream.view.clone(),
                                    project: None,
                                    upstream_view: None,
                                };
                                view = None;
                            }
                            Ok(SourceReadSelection {
                                backend: provider.backend.clone(),
                                request: original,
                                view,
                                context: Some(context),
                                public_view: Some(captured.view().clone()),
                            })
                        })
                        .await;
                }
                Err(SymbolUnavailableReason::ViewContext) => {}
                Err(_) => return Err(retained::unavailable()),
            }
        }
        let (_, selected) = self.selected_read_backend(view, None)?;
        let context = crate::proxy::candidates::read::upstream_backend(&selected);
        let mut original = request.clone();
        original.view = Some(context.view.id.clone());
        let backend = selected.project.as_deref().unwrap_or(&selected.backend);
        let backend = if backend == "global" {
            "local"
        } else {
            backend
        }
        .to_owned();
        Ok(SourceReadSelection {
            backend,
            request: original,
            view: (context.backend != "global").then(|| context.view.id.clone()),
            context: Some(context),
            public_view: Some(selected.view),
        })
    }
}

fn qualify_source_unavailable(
    acquired: super::acquire::Acquired,
    original: Option<&CapturedViewId>,
    public: Option<CapturedView>,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<super::acquire::Acquired, RiftError> {
    let super::acquire::Acquired::Response(response) = acquired else {
        return Ok(acquired);
    };
    let response = match *response {
        Ok(CallToolResponse::Complete(mut response)) => {
            if response.is_error == Some(true) {
                return Ok(super::acquire::Acquired::Response(Box::new(Ok(
                    response.into()
                ))));
            }
            let Some(value) = response.structured_content.take() else {
                return Err(retained::unavailable());
            };
            budget.charge(&value)?;
            let result: rift_protocol::query::QueryReadResult =
                serde_json::from_value(value).map_err(encoding_failure)?;
            let rift_protocol::query::QueryReadResult::Unavailable(
                rift_protocol::search_read::SearchUnavailable::Unavailable {
                    reason,
                    view,
                    warnings,
                },
            ) = result
            else {
                return Err(retained::unavailable());
            };
            if public.is_some() && view.as_ref().is_some_and(|view| Some(&view.id) != original) {
                return Err(retained::unavailable());
            }
            let result = rift_protocol::query::QueryReadResult::Unavailable(
                rift_protocol::search_read::SearchUnavailable::Unavailable {
                    reason,
                    view: public,
                    warnings,
                },
            );
            budget.charge(&result)?;
            return Ok(super::acquire::Acquired::Response(Box::new(
                crate::output::Json(result).into_call_tool_result(),
            )));
        }
        response => response,
    };
    Ok(super::acquire::Acquired::Response(Box::new(response)))
}

pub(super) fn source_identities<'a>(
    backend: &str,
    body: &crate::server::QueryFactSelection,
    sources: impl IntoIterator<
        Item = (
            &'a rift_protocol::read::SourceUnitId,
            &'a rift_protocol::read::SourceUnitId,
        ),
    >,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<rift_search::sparql::QueryIdentities, RiftError> {
    let mut identities = super::identity::identities(backend, body, budget)?;
    for (original, public) in sources {
        budget.check()?;
        let index = identities
            .sources
            .binary_search_by(|(unit, _)| unit.as_str().cmp(original.as_str()))
            .map_err(|_| retained::unavailable())?;
        budget.charge(public)?;
        identities.sources[index].1 = public.clone();
    }
    budget.check()?;
    identities
        .sources
        .sort_unstable_by(|left, right| left.1.as_str().cmp(right.1.as_str()));
    if identities
        .sources
        .windows(2)
        .any(|pair| pair[0].1 == pair[1].1)
    {
        return Err(retained::unavailable());
    }
    identities
        .sources
        .sort_unstable_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
    budget.check()?;
    Ok(identities)
}

type SourceFacts = Vec<(
    String,
    crate::server::QueryFactSelection,
    rift_search::sparql::QueryServices,
    Option<rift_search::sparql::QueryIdentities>,
)>;

fn qualify_source_facts(
    mut facts: SourceFacts,
    original: &GetSourceParams,
    public: &GetSourceParams,
    backend: &str,
    public_view: Option<&CapturedView>,
    retained_views: &crate::proxy::candidates::CandidateViews,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<super::acquire::Acquired, RiftError> {
    let mut found = false;
    for (_, body, services, identities) in &mut facts {
        budget.check()?;
        if body.services.sources.len() != 1 {
            return Err(retained::unavailable());
        }
        let mut matches = body
            .services
            .sources
            .iter()
            .filter(|(request, _)| request == original);
        let (_, result) = matches.next().ok_or_else(retained::unavailable)?;
        if found || matches.next().is_some() || !result.is_valid_for(original) {
            return Err(retained::unavailable());
        }
        found = true;
        budget.charge(result)?;
        let result = result.clone();
        let view = source_public_view(
            public_view,
            original,
            &result,
            backend,
            body,
            retained_views,
        )?;
        let qualified = qualify_source(result, original, backend, view, Some(public.unit.clone()))?;
        if !qualified.is_valid_for(public) {
            return Err(retained::unavailable());
        }
        services.sources.clear();
        budget.charge(&(public, &qualified))?;
        services.sources.push((public.clone(), qualified));
        if identities.is_none() {
            *identities = Some(source_identities(
                backend,
                body,
                std::iter::once((&original.unit, &public.unit)),
                budget,
            )?);
        }
    }
    if !found {
        return Err(retained::unavailable());
    }
    Ok(super::acquire::Acquired::Facts(facts))
}

fn source_public_view(
    public: Option<&CapturedView>,
    original: &GetSourceParams,
    result: &GetSourceResult,
    backend: &str,
    body: &crate::server::QueryFactSelection,
    retained_views: &crate::proxy::candidates::CandidateViews,
) -> Result<CapturedView, RiftError> {
    if let Some(view) = public {
        return Ok(view.clone());
    }
    let view = match result {
        GetSourceResult::Found { view, .. } | GetSourceResult::Missing { view, .. } => view.clone(),
        GetSourceResult::Unavailable { .. } => {
            return Err(retained::unavailable());
        }
    };
    let (owner, _) = rift_protocol::identity::parse_source_unit_identity(original.unit.as_str())?;
    let context = if let Some(owner) = owner {
        let mut selected = body
            .upstream
            .iter()
            .filter(|selected| selected.owners.contains(&owner));
        let selected = selected.next().ok_or_else(retained::unavailable)?;
        if selected.view != view || selected.configuration.endpoint.is_empty() {
            return Err(retained::unavailable());
        }
        crate::global::candidates::SavedCandidateBackend {
            backend: "global".to_owned(),
            root: selected.configuration.endpoint.clone(),
            owner: None,
            view: view.clone(),
            project: None,
            upstream_view: None,
        }
    } else {
        crate::global::candidates::SavedCandidateBackend {
            backend: backend.to_owned(),
            root: body.root.clone(),
            owner: Some(body.owner.clone()),
            view: view.clone(),
            project: None,
            upstream_view: None,
        }
    };
    let now = std::time::SystemTime::now();
    match retained_views
        .backend(&view.id, now)?
        .map_err(|_| retained::unavailable())?
    {
        Some(known) if known != context => return Err(retained::unavailable()),
        Some(_) => {}
        None => {
            let mut pending = retained_views.reserve_backend(
                &context.backend,
                &context.root,
                context.owner.as_ref(),
                now,
            )?;
            pending.finalize(&context, now)?;
        }
    }
    Ok(view)
}

fn captured_source_input(
    provider: &retained::Provider,
    source: &retained::Source,
    request: GetSourceParams,
    original: GetSourceParams,
    result: GetSourceResult,
    public: GetSourceResult,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<super::acquire::Acquired, RiftError> {
    budget.charge(&public)?;
    budget.charge(&(
        &provider.backend,
        &provider.root,
        &provider.owner,
        &provider.view,
        &provider.selection,
        &provider.upstream,
        &original,
    ))?;
    budget.charge_material(
        size_of::<GetSourceParams>()
            .checked_add(
                size_of::<(GetSourceParams, GetSourceResult)>()
                    .checked_mul(2)
                    .ok_or_else(retained::unavailable)?,
            )
            .ok_or_else(retained::unavailable)?,
    )?;
    let mut reads = Vec::new();
    reads
        .try_reserve_exact(1)
        .map_err(|_| retained::unavailable())?;
    reads.push(original.clone());
    let mut source_results = Vec::new();
    source_results
        .try_reserve_exact(1)
        .map_err(|_| retained::unavailable())?;
    source_results.push((original, result));
    let mut public_results = Vec::new();
    public_results
        .try_reserve_exact(1)
        .map_err(|_| retained::unavailable())?;
    public_results.push((request, public));
    let body = crate::server::QueryFactSelection {
        root: provider.root.clone(),
        owner: provider.owner.clone(),
        view: provider.view.clone(),
        selection: provider.selection.clone(),
        requests: 0,
        graph: None,
        requirements: rift_search::sparql::FactRequirements {
            source_reads: reads,
            ..Default::default()
        },
        subjects: Vec::new(),
        facts: rift_search::sparql::QueryFacts::default(),
        services: rift_search::sparql::QueryServices {
            sources: source_results,
            ..Default::default()
        },
        warnings: Vec::new(),
        upstream: provider.upstream.clone(),
    };
    body.validate_coverage(&body.requirements)?;
    let identities = source_identities(
        &provider.backend,
        &body,
        std::iter::once((&source.original, &source.public)),
        budget,
    )?;
    let services = rift_search::sparql::QueryServices {
        sources: public_results,
        ..Default::default()
    };
    budget.charge(&services)?;
    budget.charge_material(size_of::<(
        String,
        crate::server::QueryFactSelection,
        rift_search::sparql::QueryServices,
        Option<rift_search::sparql::QueryIdentities>,
    )>())?;
    let mut facts = Vec::new();
    facts
        .try_reserve_exact(1)
        .map_err(|_| retained::unavailable())?;
    facts.push((provider.backend.clone(), body, services, Some(identities)));
    Ok(super::acquire::Acquired::Facts(facts))
}

fn unheld_source_read(request: &GetSourceParams) -> Result<SourceReadSelection, RiftError> {
    let (owner, _) = rift_protocol::identity::parse_source_unit_identity(request.unit.as_str())?;
    if !request.unit.is_project() && owner.is_none() {
        return Err(retained::unavailable());
    }
    Ok(SourceReadSelection {
        backend: "local".to_owned(),
        request: request.clone(),
        view: None,
        context: None,
        public_view: None,
    })
}
