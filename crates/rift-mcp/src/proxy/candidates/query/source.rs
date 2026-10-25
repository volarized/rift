//! Exact reads use admitted query offsets or their original backend selection.

use rift_error::{RiftError, errors};
use rift_protocol::source_read::{GetSourceParams, GetSourceResult};
use rift_protocol::symbol_read::{CapturedView, CapturedViewId, SymbolUnavailableReason};
use rift_search::sparql::{PageForm, PageSelection, QueryLimits};
use rmcp::model::{CallToolRequestParams, CallToolResponse};

use super::super::RiftProxy;
use super::retained::{self, Metadata};

impl RiftProxy {
    pub(super) async fn query_material_page(
        &self,
        selection: PageSelection,
        deadline: tokio::time::Instant,
    ) -> Result<Option<CallToolResponse>, RiftError> {
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
            Ok(Some(
                rmcp::model::CallToolResult::structured(
                    serde_json::to_value(response).map_err(encoding_failure)?,
                )
                .into(),
            ))
        })
        .await
    }

    pub(in crate::proxy) async fn query_material_read(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        if !matches!(request.name.as_ref(), "get_source" | "get_symbol") {
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
                select_read(request, captured, saved, budget)
            })
            .await?;
        match selected {
            SelectedRead::Captured(value) => Ok(Some(Ok(rmcp::model::CallToolResult::structured(
                value,
            )
            .into()))),
            SelectedRead::Forward {
                backend,
                request,
                view,
                public_unit,
                source_units,
            } => {
                let response = self
                    .query_backend(&backend)?
                    .forward_read(request.clone())
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
                        public_unit,
                        &source_units,
                    )?;
                    if let Ok(CallToolResponse::Complete(response)) = &response {
                        budget.charge(response)?;
                    }
                    Ok(Some(response))
                })
                .await
            }
        }
    }

    fn query_roots(&self) -> std::collections::BTreeMap<String, String> {
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

fn validate_backends(
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

enum SelectedRead {
    Captured(serde_json::Value),
    Forward {
        backend: String,
        request: CallToolRequestParams,
        view: CapturedView,
        public_unit: Option<rift_protocol::read::SourceUnitId>,
        source_units: Vec<(
            rift_protocol::read::SourceUnitId,
            rift_protocol::read::SourceUnitId,
        )>,
    },
}

fn select_read(
    request: CallToolRequestParams,
    captured: rift_server::CapturedQueryView,
    saved: Metadata,
    budget: &mut rift_server::QueryFactBudget<'_>,
) -> Result<SelectedRead, RiftError> {
    let mut arguments = request.arguments.clone().unwrap_or_default();
    let (provider, public_unit) = if request.name == "get_symbol" {
        let id = arguments
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(retained::unavailable)?;
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
        let (provider, original) = matches.next().ok_or_else(retained::unavailable)?;
        if matches.next().is_some() {
            return Err(retained::unavailable());
        }
        arguments.insert(
            "id".to_owned(),
            serde_json::to_value(original).map_err(encoding_failure)?,
        );
        (provider, None)
    } else {
        let params: GetSourceParams =
            serde_json::from_value(arguments.clone().into()).map_err(encoding_failure)?;
        if !params.is_valid() {
            return Err(retained::unavailable());
        }
        let mut matches = saved
            .sources
            .iter()
            .filter(|source| source.public == params.unit);
        let source = matches.next().ok_or_else(retained::unavailable)?;
        if matches.next().is_some() {
            return Err(retained::unavailable());
        }
        if let Some(bytes) = &source.bytes {
            let text = std::str::from_utf8(
                captured
                    .material()
                    .sources()
                    .get(bytes.clone())
                    .ok_or_else(retained::unavailable)?,
            )
            .map_err(|_| retained::unavailable())?;
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
                view: captured.view().clone(),
                warnings: Vec::new(),
            };
            budget.charge(&response)?;
            return Ok(SelectedRead::Captured(
                serde_json::to_value(response).map_err(encoding_failure)?,
            ));
        }
        arguments.insert(
            "unit".to_owned(),
            serde_json::to_value(&source.original).map_err(encoding_failure)?,
        );
        (source.provider, Some(source.public.clone()))
    };
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
    arguments.insert(
        "view".to_owned(),
        serde_json::to_value(&provider.view.id).map_err(encoding_failure)?,
    );
    Ok(SelectedRead::Forward {
        backend: provider.backend.clone(),
        request: CallToolRequestParams::new(request.name).with_arguments(arguments),
        view: captured.view().clone(),
        public_unit,
        source_units,
    })
}

fn qualify_response(
    response: Result<CallToolResponse, rmcp::ErrorData>,
    request: &CallToolRequestParams,
    backend: &str,
    view: CapturedView,
    public_unit: Option<rift_protocol::read::SourceUnitId>,
    source_units: &[(
        rift_protocol::read::SourceUnitId,
        rift_protocol::read::SourceUnitId,
    )],
) -> Result<Result<CallToolResponse, rmcp::ErrorData>, RiftError> {
    let Ok(CallToolResponse::Complete(mut result)) = response else {
        return Ok(response);
    };
    if result.is_error == Some(true) {
        return Ok(Ok(result.into()));
    }
    let value = result
        .structured_content
        .take()
        .ok_or_else(retained::unavailable)?;
    let response = if request.name == "get_source" {
        let params: GetSourceParams =
            serde_json::from_value(request.arguments.clone().unwrap_or_default().into())
                .map_err(encoding_failure)?;
        let mut source: GetSourceResult =
            serde_json::from_value(value).map_err(encoding_failure)?;
        if !source.is_valid_for(&params) {
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
        serde_json::to_value(source).map_err(encoding_failure)?
    } else {
        let mut symbol: rift_protocol::symbol_read::GetSymbolResult =
            serde_json::from_value(value).map_err(encoding_failure)?;
        let original: rift_protocol::symbol_read::GetSymbolParams =
            serde_json::from_value(request.arguments.clone().unwrap_or_default().into())
                .map_err(encoding_failure)?;
        if !symbol.is_valid_for(&original.id) {
            return Err(retained::unavailable());
        }
        crate::proxy::identity::qualify_symbol_result(&mut symbol, backend)?;
        if backend != "local"
            && let rift_protocol::symbol_read::GetSymbolResult::Found { declarations, .. } =
                &mut symbol
        {
            for declaration in declarations {
                let Some(original) = declaration.unit.as_ref() else {
                    continue;
                };
                if let Some((_, public)) = source_units.iter().find(|(unit, _)| unit == original) {
                    if public != original {
                        declaration.unit = Some(public.clone());
                    }
                } else if rift_protocol::identity::parse_source_unit_identity(original.as_str())?
                    .0
                    .is_none()
                {
                    return Err(retained::unavailable());
                }
            }
        }
        match &mut symbol {
            rift_protocol::symbol_read::GetSymbolResult::Found { view: selected, .. }
            | rift_protocol::symbol_read::GetSymbolResult::Missing { view: selected, .. } => {
                *selected = view
            }
            rift_protocol::symbol_read::GetSymbolResult::Unavailable { view: selected, .. } => {
                *selected = Some(view)
            }
        }
        serde_json::to_value(symbol).map_err(encoding_failure)?
    };
    Ok(Ok(rmcp::model::CallToolResult::structured(response).into()))
}
