use super::{CANDIDATE_BYTES_MAX, CapturedViewId, RiftProxy, SavedCandidateBackend, invalid};
use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::search_read::SearchReadResult;
use rift_protocol::symbol_read::{CapturedView, GetSymbolParams, GetSymbolResult};
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResponse, ClientRequest, ServerResult,
};
use std::time::SystemTime;

mod dependency;

impl RiftProxy {
    pub(in crate::proxy) async fn captured_read(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let arguments = request.arguments.clone().unwrap_or_default();
        match request.name.as_ref() {
            "get_symbol" => self.captured_symbol(request).await,
            "search" if arguments.contains_key("traversal") && !arguments.contains_key("view") => {
                self.captured_traversal(request).await
            }
            "search" if !arguments.contains_key("view") => self.project_search(request).await,
            "nodes" if !arguments.contains_key("view") => self.project_nodes(request).await,
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
            let expected = dependency::upstream_backend(&selected);
            let upstream =
                dependency::upstream_request(symbol_request(request, &dispatched)?, &selected)?;
            if selected.upstream_view.is_some() {
                dispatched.view = Some(expected.view.id.clone());
            }
            let mut response = backend.forward_read(upstream).await?;
            if let Ok(CallToolResponse::Complete(result)) = &response
                && canonical_symbol_response(result)
            {
                let mut decoded = decode_symbol(result)?;
                validate_symbol(&dispatched, &decoded, Some(&expected.view))?;
                validate_global_backend(result, &selected, symbol_view(&decoded))?;
                dependency::public_symbol_view(&mut decoded, &selected);
                super::super::identity::qualify_symbol_result(&mut decoded, &selected.backend)?;
                validate_symbol(&params, &decoded, Some(&selected.view))?;
                response = symbol_response(decoded, result)?;
            }
            return Ok(Some(response));
        }
        let backend = self.call_backend(request)?;
        let key = super::super::identity::backend_key(&owner);
        let name = key.as_deref().unwrap_or("local");
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
            && canonical_symbol_response(result)
        {
            let mut decoded = decode_symbol(result)?;
            validate_symbol(&dispatched, &decoded, None)?;
            if let Some(view) = symbol_view(&decoded) {
                let selected = symbol_backend(result, view, name, root, &actual_owner)?;
                let retained = self
                    .candidate_views
                    .backend(&view.id, SystemTime::now())?
                    .map_err(|reason| view_refusal(&view.id, reason))?;
                if retained.is_some() {
                    let (selected_backend, held) =
                        self.selected_read_backend(&view.id, Some(&owner))?;
                    if !std::ptr::eq(selected_backend, backend) || held != selected {
                        return Err(view_refusal(
                            &view.id,
                            rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
                        ));
                    }
                    validate_symbol(&dispatched, &decoded, Some(&selected.view))?;
                } else if selected.backend == "global" {
                    drop(reservation);
                    let mut accepted = self.candidate_views.reserve_backend(
                        &selected.backend,
                        &selected.root,
                        selected.owner.as_ref(),
                        SystemTime::now(),
                    )?;
                    accepted.finalize(&selected, SystemTime::now())?;
                } else {
                    reservation.finalize(&selected, SystemTime::now())?;
                }
            }
            super::super::identity::qualify_symbol_result(&mut decoded, name)?;
            validate_symbol(&params, &decoded, None)?;
            response = symbol_response(decoded, result)?;
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
        let key = super::super::identity::backend_key(&owner);
        let name = key
            .as_deref()
            .unwrap_or(if matches!(owner, SymbolOwner::Local) {
                "local"
            } else {
                "global"
            });
        let actual_seed = super::super::identity::dispatch_id(seed, name)?;
        let actual_owner = SymbolIdentity::parse(actual_seed.as_str())?.owner().clone();
        self.capture_project_search(request, name, actual_owner)
            .await
    }

    async fn project_search(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let arguments = request.arguments.clone().unwrap_or_default();
        if !arguments.contains_key("pattern") && !arguments.contains_key("change") {
            return Ok(None);
        }
        super::super::encode_bounded(&arguments, CANDIDATE_BYTES_MAX)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        let params: rift_protocol::read::SearchParams = serde_json::from_value(arguments.into())
            .map_err(|source| {
                errors::mcp::parameter_invalid()
                    .tool("search")
                    .field("arguments")
                    .accepted(source.to_string())
                    .error()
            })?;
        // Revision-addressed reads keep the server's existing scope refusal.
        if params.revision.is_some() {
            return Ok(None);
        }
        let Some(name) = super::super::identity::scope_key(&params.scope) else {
            return Ok(None);
        };
        self.project_read(request, &name).await.map(Some)
    }

    /// Reads the nodes of a symbol an accepted extra project or profile owns from that
    /// project's own server, which knows the symbol under its `local` owner.
    async fn project_nodes(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let arguments = request.arguments.clone().unwrap_or_default();
        let Some(symbol) = arguments.get("symbol").and_then(serde_json::Value::as_str) else {
            return Ok(None);
        };
        let owner = SymbolIdentity::parse(symbol)?.owner().clone();
        let Some(name) = super::super::identity::backend_key(&owner) else {
            return Ok(None);
        };
        self.project_read(request, &name).await.map(Some)
    }

    /// Forwards one read to the backend `request` selects with its identities dispatched to
    /// that backend, and qualifies the identities of the answer with `name` again.
    async fn project_read(
        &self,
        request: &CallToolRequestParams,
        name: &str,
    ) -> Result<Result<CallToolResponse, rmcp::ErrorData>, RiftError> {
        let backend = self.call_backend(request)?;
        let upstream = dispatch_read_request(request, name)?;
        let mut response = backend.forward_read(upstream).await?;
        if let Ok(CallToolResponse::Complete(result)) = &response
            && result.is_error != Some(true)
        {
            super::super::encode_bounded(result, CANDIDATE_BYTES_MAX)
                .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
            let value = result.structured_content.clone().ok_or_else(|| {
                invalid(
                    "structuredContent",
                    "project response has no structured result",
                )
            })?;
            let meta = result.meta.clone();
            response = qualified_read_response(request, value, name)?;
            if let Ok(CallToolResponse::Complete(answered)) = &mut response {
                answered.meta = meta;
            }
        }
        Ok(response)
    }

    async fn capture_project_search(
        &self,
        request: &CallToolRequestParams,
        name: &str,
        actual_owner: SymbolOwner,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let backend = self.call_backend(request)?;
        let root = accepted_root(backend)?;
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
                        project: None,
                        upstream_view: None,
                    },
                    SystemTime::now(),
                )?;
            }
            let value = self.dependency_views(result, value.clone(), name, root)?;
            let meta = result.meta.clone();
            response = qualified_read_response(request, value, name)?;
            if let Ok(CallToolResponse::Complete(answered)) = &mut response {
                answered.meta = meta;
            }
        }
        Ok(Some(response))
    }

    async fn held_read(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let arguments = request.arguments.clone().unwrap_or_default();
        if !matches!(request.name.as_ref(), "nodes" | "search") {
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
            .or_else(|| arguments.get("symbol"))
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
        let upstream = dependency::upstream_request(
            dispatch_read_request(request, &selected.backend)?,
            &selected,
        )?;
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
            let expected = dependency::upstream_backend(&selected);
            if returned.as_ref().is_some_and(|view| view != &expected.view) {
                return Err(invalid("view", "captured backend context does not match"));
            }
            let mut value = value.clone();
            if selected.upstream_view.is_some() && returned.is_some() {
                value["view"] = serde_json::json!(selected.view);
            }
            if request.name == "search" {
                value = self.dependency_views(result, value, &selected.backend, &selected.root)?;
            }
            let mut qualified = qualified_read_response(request, value, &selected.backend)?;
            if let Ok(CallToolResponse::Complete(answered)) = &mut qualified {
                answered.meta.clone_from(&result.meta);
            }
            response = qualified;
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
        let name = selected.project.as_deref().unwrap_or(&selected.backend);
        let backend = match name {
            "local" | "global" => self,
            name => self.backends.get(name).ok_or_else(|| {
                view_refusal(
                    view,
                    rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
                )
            })?,
        };
        if selected.backend != "global" && accepted_root(backend)? != selected.root {
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

pub(in crate::proxy) fn dispatch_read_request(
    request: &CallToolRequestParams,
    backend: &str,
) -> Result<CallToolRequestParams, RiftError> {
    if request.name == "nodes" {
        return dispatch_nodes_request(request, backend);
    }
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
    if let Some(name) = super::super::identity::scope_key(&params.scope) {
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

/// `request` with its `symbol` dispatched to `backend`'s own `local` owner.
fn dispatch_nodes_request(
    request: &CallToolRequestParams,
    backend: &str,
) -> Result<CallToolRequestParams, RiftError> {
    let mut params: rift_protocol::read::NodesParams = serde_json::from_value(
        request.arguments.clone().unwrap_or_default().into(),
    )
    .map_err(|source| {
        errors::mcp::parameter_invalid()
            .tool("nodes")
            .field("arguments")
            .accepted(source.to_string())
            .error()
    })?;
    if let Some(symbol) = &mut params.symbol {
        *symbol = super::super::identity::dispatch_id(symbol, backend)?;
    }
    let value = serde_json::to_value(params)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    let mut upstream = request.clone();
    upstream.arguments = Some(
        value
            .as_object()
            .cloned()
            .ok_or_else(|| invalid("arguments", "nodes request is not an object"))?,
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

fn canonical_symbol_response(result: &rmcp::model::CallToolResult) -> bool {
    result.is_error != Some(true)
        || result.structured_content.as_ref().is_some_and(|value| {
            value.get("outcome").and_then(serde_json::Value::as_str) == Some("missing")
        })
}

fn symbol_response(
    decoded: GetSymbolResult,
    original: &rmcp::model::CallToolResult,
) -> Result<Result<CallToolResponse, rmcp::ErrorData>, RiftError> {
    let response = crate::output::Json(decoded)
        .into_call_tool_result()
        .map(|mut response| {
            if let CallToolResponse::Complete(result) = &mut response {
                result.meta.clone_from(&original.meta);
            }
            response
        });
    bounded_response(response)
}

fn symbol_backend(
    result: &rmcp::model::CallToolResult,
    view: &CapturedView,
    backend: &str,
    root: &str,
    owner: &SymbolOwner,
) -> Result<SavedCandidateBackend, RiftError> {
    let global = matches!(
        owner,
        SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
    );
    let Some(meta) = result.meta.as_ref().filter(|meta| {
        ["backend", "root", "owner", "view"]
            .iter()
            .any(|field| meta.contains_key(*field))
    }) else {
        if global {
            return Err(view_refusal(
                &view.id,
                rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
            ));
        }
        return Ok(SavedCandidateBackend {
            backend: backend.to_owned(),
            root: root.to_owned(),
            owner: Some(owner.clone()),
            view: view.clone(),
            project: None,
            upstream_view: None,
        });
    };
    if !global {
        return Err(view_refusal(
            &view.id,
            rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
        ));
    }
    global_backend(meta, view)
}

fn global_backend(
    meta: &rmcp::model::MetaObject,
    view: &CapturedView,
) -> Result<SavedCandidateBackend, RiftError> {
    let mut context = serde_json::Map::new();
    for field in ["backend", "root", "owner", "view"] {
        let value = meta.get(field).ok_or_else(|| {
            view_refusal(
                &view.id,
                rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
            )
        })?;
        context.insert(field.to_owned(), value.clone());
    }
    super::super::encode_bounded(&context, CANDIDATE_BYTES_MAX)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    let selected: SavedCandidateBackend = serde_json::from_value(context.into())
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    if selected.backend != "global"
        || selected.owner.is_some()
        || selected.root.is_empty()
        || selected.view != *view
    {
        return Err(view_refusal(
            &view.id,
            rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
        ));
    }
    if super::captured_expiry(view)? <= SystemTime::now() {
        return Err(view_refusal(
            &view.id,
            rift_protocol::symbol_read::SymbolUnavailableReason::ViewExpired,
        ));
    }
    Ok(selected)
}

fn validate_global_backend(
    result: &rmcp::model::CallToolResult,
    selected: &SavedCandidateBackend,
    returned: Option<&CapturedView>,
) -> Result<(), RiftError> {
    if selected.backend != "global" {
        return Ok(());
    }
    let Some(returned) = returned else {
        return Ok(());
    };
    let meta = result.meta.as_ref().ok_or_else(|| {
        view_refusal(
            &selected.view.id,
            rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
        )
    })?;
    if global_backend(meta, returned)? != dependency::upstream_backend(selected) {
        return Err(view_refusal(
            &selected.view.id,
            rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
        ));
    }
    Ok(())
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
                    if result.results.iter().any(|hit| hit.view.is_none()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResult, MetaObject};

    #[test]
    fn selected_search_dispatch_preserves_pattern_change_and_scope_boundary()
    -> Result<(), Box<dyn std::error::Error>> {
        for (scope, backend, foreign) in [
            ("local@docs", "docs", ":docs"),
            ("local:docs", ":docs", "docs"),
        ] {
            for selector in [
                serde_json::json!({"pattern":"beacon", "target":"symbol"}),
                serde_json::json!({"change":{"base":"HEAD","head":{"kind":"working_tree"}}}),
            ] {
                let mut arguments = selector.as_object().ok_or("selector object")?.clone();
                arguments.insert("scope".to_owned(), serde_json::json!(scope));
                let request = CallToolRequestParams::new("search").with_arguments(arguments);
                let dispatched = dispatch_read_request(&request, backend)?;
                let original: rift_protocol::read::SearchParams =
                    serde_json::from_value(request.arguments.clone().ok_or("arguments")?.into())?;
                let selected: rift_protocol::read::SearchParams = serde_json::from_value(
                    dispatched.arguments.ok_or("dispatched arguments")?.into(),
                )?;
                assert_eq!(selected.scope, rift_protocol::read::SearchScope::Local);
                assert_eq!(selected.pattern, original.pattern);
                assert_eq!(selected.change, original.change);
                assert!(dispatch_read_request(&request, foreign).is_err());
            }
        }
        Ok(())
    }

    fn owner() -> SymbolOwner {
        SymbolOwner::Package {
            manager: "cargo".to_owned(),
            registry: "crates.io".to_owned(),
            name: "demo".to_owned(),
            version: "1.0.0".to_owned(),
        }
    }

    fn view() -> CapturedView {
        CapturedView {
            id: CapturedViewId::parse("claims.global").expect("view"),
            expires_at: "2030-01-01T00:15:00+00:00".to_owned(),
        }
    }

    fn response(view: &CapturedView) -> CallToolResult {
        let mut meta = MetaObject::new();
        meta.insert("backend".to_owned(), serde_json::json!("global"));
        meta.insert("root".to_owned(), serde_json::json!("/fixture/global"));
        meta.insert("owner".to_owned(), serde_json::Value::Null);
        meta.insert("view".to_owned(), serde_json::json!(view));
        CallToolResult::success(Vec::new()).with_meta(Some(meta))
    }

    #[test]
    fn symbol_backend_retains_global_context_and_original_view() {
        let view = view();
        let mut response = response(&view);
        response
            .meta
            .as_mut()
            .expect("metadata")
            .insert("traceparent".to_owned(), serde_json::json!("captured"));
        let selected = symbol_backend(&response, &view, "local", "/fixture/main", &owner())
            .expect("global context");
        assert_eq!(selected.backend, "global");
        assert_eq!(selected.root, "/fixture/global");
        assert_eq!(selected.owner, None);
        assert_eq!(selected.view, view);
    }

    #[test]
    fn validate_global_backend_retains_original_frame_and_unavailable_without_view() {
        let view = view();
        let response = response(&view);
        let selected = symbol_backend(&response, &view, "local", "/fixture/main", &owner())
            .expect("global context");
        assert!(validate_global_backend(&response, &selected, Some(&view)).is_ok());
        let unavailable = CallToolResult::success(Vec::new());
        assert!(validate_global_backend(&unavailable, &selected, None).is_ok());
    }

    #[test]
    fn validate_global_backend_refuses_changed_endpoint_and_missing_held_metadata() {
        let view = view();
        let response = response(&view);
        let selected = symbol_backend(&response, &view, "local", "/fixture/main", &owner())
            .expect("global context");
        let mut changed = response.clone();
        changed
            .meta
            .as_mut()
            .expect("metadata")
            .insert("root".to_owned(), serde_json::json!("/fixture/foreign"));
        assert!(validate_global_backend(&changed, &selected, Some(&view)).is_err());
        let mut missing = response.clone();
        missing.meta = None;
        assert!(validate_global_backend(&missing, &selected, Some(&view)).is_err());
    }

    #[test]
    fn symbol_backend_refuses_missing_global_context() {
        let view = view();
        assert!(
            symbol_backend(
                &CallToolResult::success(Vec::new()),
                &view,
                "local",
                "/fixture/main",
                &owner(),
            )
            .is_err()
        );
        for field in ["backend", "root", "owner", "view"] {
            let mut response = response(&view);
            response.meta.as_mut().expect("metadata").remove(field);
            assert!(symbol_backend(&response, &view, "local", "/fixture/main", &owner()).is_err());
        }
    }

    #[test]
    fn symbol_backend_refuses_changed_backend_root_owner_or_view() {
        let view = view();
        for (field, value) in [
            ("backend", serde_json::json!("local")),
            ("root", serde_json::json!("")),
            ("owner", serde_json::json!(SymbolOwner::Local)),
            (
                "view",
                serde_json::json!(CapturedView {
                    id: CapturedViewId::parse("claims.foreign").expect("foreign view"),
                    expires_at: view.expires_at.clone(),
                }),
            ),
            (
                "view",
                serde_json::json!(CapturedView {
                    id: view.id.clone(),
                    expires_at: "2030-01-01T00:14:00+00:00".to_owned(),
                }),
            ),
        ] {
            let mut response = response(&view);
            response
                .meta
                .as_mut()
                .expect("metadata")
                .insert(field.to_owned(), value);
            assert!(symbol_backend(&response, &view, "local", "/fixture/main", &owner()).is_err());
        }
    }

    #[test]
    fn symbol_backend_refuses_expired_or_invalid_global_context() {
        let mut expired = view();
        expired.expires_at = "2000-01-01T00:00:00+00:00".to_owned();
        assert!(
            symbol_backend(
                &response(&expired),
                &expired,
                "local",
                "/fixture/main",
                &owner()
            )
            .is_err()
        );
        let view = view();
        let mut response = response(&view);
        response
            .meta
            .as_mut()
            .expect("metadata")
            .insert("backend".to_owned(), serde_json::json!(false));
        assert!(symbol_backend(&response, &view, "local", "/fixture/main", &owner()).is_err());
    }

    #[test]
    fn symbol_backend_keeps_project_context_and_refuses_foreign_global_metadata() {
        let view = view();
        for (backend, owner) in [
            ("local", SymbolOwner::Local),
            (
                "extra",
                SymbolOwner::NamedLocal {
                    name: "extra".to_owned(),
                },
            ),
        ] {
            let response = CallToolResult::success(Vec::new());
            let selected = symbol_backend(&response, &view, backend, "/fixture/main", &owner)
                .expect("project context");
            assert_eq!(selected.backend, backend);
            assert_eq!(selected.root, "/fixture/main");
            assert_eq!(selected.owner, Some(owner.clone()));
            assert_eq!(selected.view, view);
            assert!(
                symbol_backend(
                    &self::response(&view),
                    &view,
                    backend,
                    "/fixture/main",
                    &owner
                )
                .is_err()
            );
        }
    }
}
