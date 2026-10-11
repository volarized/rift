use super::{CANDIDATE_BYTES_MAX, CapturedViewId, RiftProxy, SavedCandidateBackend, invalid};
use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::search_read::SearchReadResult;
use rift_protocol::source_read::GetSourceResult;
use rift_protocol::symbol_read::{CapturedView, GetSymbolParams, GetSymbolResult};
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResponse, ClientRequest, ServerResult,
};
use std::time::SystemTime;

impl RiftProxy {
    pub(in crate::proxy) async fn captured_read(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let arguments = request.arguments.clone().unwrap_or_default();
        match request.name.as_ref() {
            "get_symbol" => self.captured_symbol(request).await,
            "get_source" if !arguments.contains_key("view") => self.captured_source(request).await,
            "search" if arguments.contains_key("traversal") && !arguments.contains_key("view") => {
                self.captured_traversal(request).await
            }
            _ => self.held_read(request).await,
        }
    }

    async fn captured_symbol(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let arguments = request.arguments.clone().unwrap_or_default();
        super::super::encode_bounded(&arguments, CANDIDATE_BYTES_MAX)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        let params: GetSymbolParams = match serde_json::from_value(arguments.clone().into()) {
            Ok(params) => params,
            Err(_) => {
                // The owning router supplies its cached canonical examples before the
                // typed method is entered. A malformed request cannot issue a view.
                return Ok(Some(self.forward_read(request.clone()).await?));
            }
        };
        let owner = SymbolIdentity::parse(params.id.as_str())?.owner().clone();
        if let Some(view) = params.view.as_ref() {
            let (backend, selected) = self.selected_read_backend(view, Some(&owner))?;
            let mut dispatched = params.clone();
            dispatched.id = super::super::identity::dispatch_id(&params.id, &selected.backend)?;
            let upstream = symbol_request(request, &dispatched)?;
            let mut response = backend.forward_read(upstream).await?;
            if let Ok(CallToolResponse::Complete(result)) = &response
                && result.is_error != Some(true)
            {
                let mut decoded = decode_symbol(result)?;
                validate_symbol(&dispatched, &decoded, Some(&selected.view))?;
                super::super::identity::qualify_symbol_result(&mut decoded, &selected.backend)?;
                validate_symbol(&params, &decoded, Some(&selected.view))?;
                response = bounded_response(crate::output::Json(decoded).into_call_tool_result())?;
            }
            return Ok(Some(response));
        }
        let backend = self.call_backend(request)?;
        let name = match &owner {
            SymbolOwner::NamedLocal { name } => name.as_str(),
            _ => "local",
        };
        let root = accepted_root(backend)?;
        let mut dispatched = params.clone();
        dispatched.id = super::super::identity::dispatch_id(&params.id, name)?;
        let actual_owner = SymbolIdentity::parse(dispatched.id.as_str())?
            .owner()
            .clone();
        let upstream = symbol_request(request, &dispatched)?;
        let mut reservation =
            self.candidate_views
                .reserve_symbol(name, root, &actual_owner, SystemTime::now())?;
        let mut response = backend.forward_read(upstream).await?;
        if let Ok(CallToolResponse::Complete(result)) = &response
            && result.is_error != Some(true)
        {
            let mut decoded = decode_symbol(result)?;
            validate_symbol(&dispatched, &decoded, None)?;
            if let Some(view) = symbol_view(&decoded) {
                reservation.finalize(
                    &SavedCandidateBackend {
                        backend: name.to_owned(),
                        root: root.to_owned(),
                        owner: Some(actual_owner),
                        view: view.clone(),
                    },
                    SystemTime::now(),
                )?;
            }
            super::super::identity::qualify_symbol_result(&mut decoded, name)?;
            validate_symbol(&params, &decoded, None)?;
            response = bounded_response(crate::output::Json(decoded).into_call_tool_result())?;
        }
        Ok(Some(response))
    }

    async fn captured_source(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let arguments = request.arguments.clone().unwrap_or_default();
        super::super::encode_bounded(&arguments, CANDIDATE_BYTES_MAX)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        let params: rift_protocol::source_read::GetSourceParams =
            serde_json::from_value(arguments.clone().into()).map_err(|source| {
                errors::mcp::parameter_invalid()
                    .tool("get_source")
                    .field("arguments")
                    .accepted(source.to_string())
                    .error()
            })?;
        if !params.is_valid() {
            return Err(invalid(
                "source",
                "source selectors or byte range are invalid",
            ));
        }
        let (owner, _) = rift_protocol::identity::parse_source_unit_identity(params.unit.as_str())?;
        let owner = owner.unwrap_or(SymbolOwner::Local);
        let backend = self;
        let root = accepted_root(backend)?;
        let mut reservation =
            self.candidate_views
                .reserve_symbol("local", root, &owner, SystemTime::now())?;
        let response = backend.forward_read(request.clone()).await?;
        if let Ok(CallToolResponse::Complete(result)) = &response
            && result.is_error != Some(true)
        {
            super::super::encode_bounded(result, CANDIDATE_BYTES_MAX)
                .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
            let value = result.structured_content.as_ref().ok_or_else(|| {
                invalid(
                    "structuredContent",
                    "captured response has no structured result",
                )
            })?;
            if let Some(view) = returned_view(request, value.clone())? {
                reservation.finalize(
                    &SavedCandidateBackend {
                        backend: "local".to_owned(),
                        root: root.to_owned(),
                        owner: Some(owner),
                        view,
                    },
                    SystemTime::now(),
                )?;
            }
        }
        Ok(Some(response))
    }

    async fn captured_traversal(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let arguments = request.arguments.clone().unwrap_or_default();
        super::super::encode_bounded(&arguments, CANDIDATE_BYTES_MAX)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        let params: rift_protocol::read::SearchParams =
            serde_json::from_value(arguments.clone().into()).map_err(|source| {
                errors::mcp::parameter_invalid()
                    .tool("search")
                    .field("arguments")
                    .accepted(source.to_string())
                    .error()
            })?;
        let seed = params
            .traversal
            .as_ref()
            .and_then(|traversal| traversal.seed.as_ref())
            .ok_or_else(|| invalid("seed", "a traversal starts at seed"))?;
        let owner = SymbolIdentity::parse(seed.as_str())?.owner().clone();
        let name = match &owner {
            SymbolOwner::NamedLocal { name } => name.as_str(),
            SymbolOwner::Local => "local",
            SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. } => "global",
        };
        let backend = self.call_backend(request)?;
        let root = accepted_root(backend)?;
        let actual_seed = super::super::identity::dispatch_id(seed, name)?;
        let actual_owner = SymbolIdentity::parse(actual_seed.as_str())?.owner().clone();
        let upstream = dispatch_read_request(request, name)?;
        let mut reservation =
            self.candidate_views
                .reserve_symbol(name, root, &actual_owner, SystemTime::now())?;
        let mut response = backend.forward_read(upstream.clone()).await?;
        if let Ok(CallToolResponse::Complete(result)) = &response
            && result.is_error != Some(true)
        {
            super::super::encode_bounded(result, CANDIDATE_BYTES_MAX)
                .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
            let value = result.structured_content.as_ref().ok_or_else(|| {
                invalid(
                    "structuredContent",
                    "captured response has no structured result",
                )
            })?;
            if let Some(view) = returned_view(&upstream, value.clone())? {
                reservation.finalize(
                    &SavedCandidateBackend {
                        backend: name.to_owned(),
                        root: root.to_owned(),
                        owner: Some(actual_owner),
                        view,
                    },
                    SystemTime::now(),
                )?;
            }
            response = qualified_read_response(request, value.clone(), name)?;
        }
        Ok(Some(response))
    }

    async fn held_read(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let arguments = request.arguments.clone().unwrap_or_default();
        if !matches!(request.name.as_ref(), "get_source" | "nodes" | "search") {
            return Ok(None);
        }
        if request.name == "search" && !arguments.contains_key("traversal") {
            return Ok(None);
        }
        let Some(value) = arguments.get("view") else {
            return Ok(None);
        };
        let view: CapturedViewId = serde_json::from_value(value.clone()).map_err(|source| {
            errors::mcp::parameter_invalid()
                .tool(request.name.as_ref())
                .field("view")
                .accepted(source.to_string())
                .error()
        })?;
        let owner = arguments
            .get("id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                arguments
                    .get("traversal")
                    .and_then(|value| value.get("seed"))
                    .and_then(serde_json::Value::as_str)
            })
            .map(SymbolIdentity::parse)
            .transpose()?
            .map(|identity| identity.owner().clone());
        let (backend, selected) = self.selected_read_backend(&view, owner.as_ref())?;
        let upstream = dispatch_read_request(request, &selected.backend)?;
        let mut response = backend.forward_read(upstream.clone()).await?;
        if let Ok(CallToolResponse::Complete(result)) = &response
            && result.is_error != Some(true)
        {
            super::super::encode_bounded(result, CANDIDATE_BYTES_MAX)
                .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
            let value = result.structured_content.as_ref().ok_or_else(|| {
                invalid(
                    "structuredContent",
                    "captured response has no structured result",
                )
            })?;
            let returned = returned_view(&upstream, value.clone())?;
            if returned.as_ref().is_some_and(|view| view != &selected.view) {
                return Err(invalid("view", "captured backend context does not match"));
            }
            response = qualified_read_response(request, value.clone(), &selected.backend)?;
        }
        Ok(Some(response))
    }

    fn selected_read_backend<'a>(
        &'a self,
        view: &CapturedViewId,
        owner: Option<&SymbolOwner>,
    ) -> Result<(&'a Self, SavedCandidateBackend), RiftError> {
        let selected = self
            .candidate_views
            .backend(view, SystemTime::now())?
            .map_err(|reason| view_refusal(view, reason))?
            .ok_or_else(|| {
                view_refusal(
                    view,
                    rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
                )
            })?;
        if let (Some(requested), Some(actual)) = (owner, selected.owner.as_ref())
            && requested != &super::super::identity::public_owner(actual, &selected.backend)?
        {
            return Err(view_refusal(
                view,
                rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
            ));
        }
        let backend = match selected.backend.as_str() {
            "local" | "global" => self,
            name => self.backends.get(name).ok_or_else(|| {
                view_refusal(
                    view,
                    rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
                )
            })?,
        };
        if accepted_root(backend)? != selected.root {
            return Err(view_refusal(
                view,
                rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
            ));
        }
        Ok((backend, selected))
    }

    async fn forward_read(
        &self,
        request: CallToolRequestParams,
    ) -> Result<Result<CallToolResponse, rmcp::ErrorData>, RiftError> {
        self.forward_registered(
            ClientRequest::CallToolRequest(CallToolRequest::new(request)),
            |result| match result {
                ServerResult::CallToolResult(result) => Some(CallToolResponse::Complete(result)),
                ServerResult::InputRequiredResult(result) => {
                    Some(CallToolResponse::InputRequired(result))
                }
                ServerResult::CreateTaskResult(result) => Some(CallToolResponse::Task(result)),
                _ => None,
            },
        )
        .await
    }
}

fn dispatch_read_request(
    request: &CallToolRequestParams,
    backend: &str,
) -> Result<CallToolRequestParams, RiftError> {
    if request.name != "search" {
        return Ok(request.clone());
    }
    let mut params: rift_protocol::read::SearchParams =
        serde_json::from_value(request.arguments.clone().unwrap_or_default().into())
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    if let Some(traversal) = &mut params.traversal {
        if let Some(seed) = &mut traversal.seed {
            *seed = super::super::identity::dispatch_id(seed, backend)?;
        }
        if let Some(target) = &mut traversal.to {
            *target = super::super::identity::dispatch_id(target, backend)?;
        }
    }
    if let rift_protocol::read::SearchScope::NamedLocal { name } = &params.scope {
        if name != backend {
            return Err(invalid("scope", "captured backend context does not match"));
        }
        params.scope = rift_protocol::read::SearchScope::Local;
    }
    let mut upstream = request.clone();
    let value = serde_json::to_value(params)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    upstream.arguments = Some(
        value
            .as_object()
            .cloned()
            .ok_or_else(|| invalid("arguments", "search request is not an object"))?,
    );
    Ok(upstream)
}

fn qualified_read_response(
    request: &CallToolRequestParams,
    value: serde_json::Value,
    backend: &str,
) -> Result<Result<CallToolResponse, rmcp::ErrorData>, RiftError> {
    use rmcp::handler::server::tool::IntoCallToolResult;
    match request.name.as_ref() {
        "nodes" => {
            let mut result: rift_protocol::read::NodesResult = serde_json::from_value(value)
                .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
            super::super::identity::qualify_nodes(&mut result, backend)?;
            bounded_response(crate::output::Json(result).into_call_tool_result())
        }
        "search" => {
            let mut result: rift_protocol::search_read::SearchReadResult =
                serde_json::from_value(value)
                    .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
            super::super::identity::qualify_search(&mut result, backend)?;
            bounded_response(crate::output::Json(result).into_call_tool_result())
        }
        "get_source" => {
            let mut result: rift_protocol::source_read::GetSourceResult =
                serde_json::from_value(value)
                    .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
            super::super::identity::qualify_source(&mut result, backend)?;
            bounded_response(crate::output::Json(result).into_call_tool_result())
        }
        _ => Err(invalid("view", "captured backend mapping is unavailable")),
    }
}

fn bounded_response(
    response: Result<CallToolResponse, rmcp::ErrorData>,
) -> Result<Result<CallToolResponse, rmcp::ErrorData>, RiftError> {
    if let Ok(CallToolResponse::Complete(result)) = &response {
        super::super::encode_bounded(result, CANDIDATE_BYTES_MAX)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    }
    Ok(response)
}

fn symbol_request(
    request: &CallToolRequestParams,
    params: &GetSymbolParams,
) -> Result<CallToolRequestParams, RiftError> {
    let mut upstream = request.clone();
    let value = serde_json::to_value(params)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    upstream.arguments = Some(
        value
            .as_object()
            .cloned()
            .ok_or_else(|| invalid("arguments", "symbol request is not an object"))?,
    );
    Ok(upstream)
}

fn accepted_root(backend: &RiftProxy) -> Result<&str, RiftError> {
    backend
        .accepted_root
        .as_deref()
        .and_then(std::path::Path::to_str)
        .ok_or_else(|| invalid("root", "accepted project root is unavailable"))
}

fn decode_symbol(result: &rmcp::model::CallToolResult) -> Result<GetSymbolResult, RiftError> {
    super::super::encode_bounded(result, CANDIDATE_BYTES_MAX)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    let value = result.structured_content.clone().ok_or_else(|| {
        invalid(
            "structuredContent",
            "captured response has no structured result",
        )
    })?;
    serde_json::from_value(value)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())
}

fn symbol_view(result: &GetSymbolResult) -> Option<&CapturedView> {
    match result {
        GetSymbolResult::Found { view, .. } | GetSymbolResult::Missing { view, .. } => Some(view),
        GetSymbolResult::Unavailable { view, .. } => view.as_ref(),
    }
}

fn validate_symbol(
    params: &GetSymbolParams,
    result: &GetSymbolResult,
    expected: Option<&CapturedView>,
) -> Result<(), RiftError> {
    if !result.is_valid_for(&params.id)
        || expected.is_some_and(|view| symbol_view(result).is_some_and(|returned| returned != view))
    {
        return Err(invalid("view", "captured backend context does not match"));
    }
    Ok(())
}

fn returned_view(
    request: &CallToolRequestParams,
    value: serde_json::Value,
) -> Result<Option<CapturedView>, RiftError> {
    match request.name.as_ref() {
        "get_source" => {
            let params: rift_protocol::source_read::GetSourceParams =
                serde_json::from_value(request.arguments.clone().unwrap_or_default().into())
                    .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
            let result: rift_protocol::source_read::GetSourceResult = serde_json::from_value(value)
                .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
            if !result.is_valid_for(&params) {
                return Err(invalid("view", "captured backend context does not match"));
            }
            Ok(match result {
                GetSourceResult::Found { view, .. } | GetSourceResult::Missing { view, .. } => {
                    Some(view)
                }
                GetSourceResult::Unavailable { view, .. } => view,
            })
        }
        "nodes" => {
            let result: rift_protocol::read::NodesResult = serde_json::from_value(value)
                .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
            result
                .view
                .map(Some)
                .ok_or_else(|| invalid("view", "captured response has no selected view"))
        }
        "search" => {
            let result: rift_protocol::search_read::SearchReadResult =
                serde_json::from_value(value)
                    .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
            match result {
                SearchReadResult::Results(result) => {
                    let view = result
                        .view
                        .ok_or_else(|| invalid("view", "captured response has no selected view"))?;
                    if result
                        .results
                        .iter()
                        .any(|hit| hit.view.as_ref() != Some(&view))
                    {
                        return Err(invalid("view", "captured backend context does not match"));
                    }
                    Ok(Some(view))
                }
                SearchReadResult::Unavailable(
                    rift_protocol::search_read::SearchUnavailable::Unavailable { view, .. },
                ) => Ok(view),
            }
        }
        _ => Err(invalid("view", "captured backend mapping is unavailable")),
    }
}

fn view_refusal(
    view: &CapturedViewId,
    reason: rift_protocol::symbol_read::SymbolUnavailableReason,
) -> RiftError {
    use rift_protocol::symbol_read::SymbolUnavailableReason;
    match reason {
        SymbolUnavailableReason::ViewExpired => errors::server::search_view_expired()
            .operation("captured read")
            .view(view.as_str())
            .error(),
        SymbolUnavailableReason::ViewContext => errors::server::search_view_context()
            .operation("captured read")
            .view(view.as_str())
            .error(),
        _ => errors::server::search_coverage_unavailable()
            .operation("captured read")
            .view(view.as_str())
            .error(),
    }
}
