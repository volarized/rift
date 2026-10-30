use super::{CANDIDATE_BYTES_MAX, RiftProxy, invalid};
use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::symbol_read::{GetSymbolParams, GetSymbolResult};
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResponse, ClientRequest, ServerResult,
};

impl RiftProxy {
    /// Routes one read to the project whose owner its identities or scope name, with those
    /// identities dispatched to that project's own `local` owner. `None` leaves the request
    /// to the next route.
    pub(in crate::proxy) async fn routed_read(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let arguments = request.arguments.clone().unwrap_or_default();
        match request.name.as_ref() {
            "get_symbol" => self.symbol_read(request).await,
            "search" if arguments.contains_key("traversal") => self.traversal_read(request).await,
            "search" => self.project_search(request).await,
            "nodes" => self.project_nodes(request).await,
            _ => Ok(None),
        }
    }

    async fn symbol_read(
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
                // typed method is entered.
                return Ok(Some(self.forward_read(request.clone()).await?));
            }
        };
        let owner = SymbolIdentity::parse(params.id.as_str())?.owner().clone();
        let backend = self.call_backend(request)?;
        let key = super::super::identity::backend_key(&owner);
        let name = key.as_deref().unwrap_or("local");
        let mut dispatched = params.clone();
        dispatched.id = super::super::identity::dispatch_id(&params.id, name)?;
        let mut response = backend
            .forward_read(symbol_request(request, &dispatched)?)
            .await?;
        if let Ok(CallToolResponse::Complete(result)) = &response
            && canonical_symbol_response(result)
        {
            let mut decoded = decode_symbol(result)?;
            validate_symbol(&dispatched, &decoded)?;
            super::super::identity::qualify_symbol_result(&mut decoded, name)?;
            validate_symbol(&params, &decoded)?;
            response = symbol_response(decoded, result)?;
        }
        Ok(Some(response))
    }

    async fn traversal_read(
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
        self.project_read(request, name).await.map(Some)
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

fn validate_symbol(params: &GetSymbolParams, result: &GetSymbolResult) -> Result<(), RiftError> {
    if !result.is_valid_for(&params.id) {
        return Err(invalid("id", "the project answered another symbol"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
