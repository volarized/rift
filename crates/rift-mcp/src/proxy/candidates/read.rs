use super::{CANDIDATE_BYTES_MAX, RiftProxy, invalid};
use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, parse_local_scope};
use rift_protocol::symbol_read::{GetSymbolParams, GetSymbolResult};
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResponse, ClientRequest, ServerResult,
};

use crate::parameters::NodesToolParams;

impl RiftProxy {
    /// Routes one read to the project whose owner its identities or scope name, with those
    /// identities dispatched to that project's own `local` owner. `None` leaves the request
    /// to the next route.
    pub(in crate::proxy) async fn routed_read(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        match request.name.as_ref() {
            "get_symbol" => self.symbol_read(request).await,
            "search" => self.query_forward(request).await.map(Some),
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

    /// Reads the nodes of an accepted project or profile from its own server.
    async fn project_nodes(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let Some(arguments) = request.arguments.as_ref() else {
            return Ok(None);
        };
        if let Some(scope) = arguments.get("scope") {
            let owner = scope
                .as_str()
                .and_then(|scope| parse_local_scope(scope).ok())
                .ok_or_else(nodes_scope_refusal)?;
            if arguments.contains_key("symbol")
                || !arguments
                    .get("path")
                    .is_some_and(serde_json::Value::is_string)
                || !arguments
                    .get("position")
                    .is_some_and(serde_json::Value::is_u64)
            {
                return Err(nodes_scope_refusal());
            }
            let Some(name) = super::super::identity::backend_key(&owner) else {
                return Ok(None);
            };
            return self.project_read(request, &name).await.map(Some);
        }
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
        let backend = self
            .backends
            .get(name)
            .ok_or_else(|| self.extra.refusal(name, &request.name, "scope"))?;
        let upstream = dispatch_nodes_request(request, name)?;
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
            response = qualified_nodes_response(value, name)?;
            if let Ok(CallToolResponse::Complete(answered)) = &mut response {
                answered.meta = meta;
            }
        }
        Ok(response)
    }

    pub(in crate::proxy) async fn forward_read(
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

fn nodes_scope_refusal() -> RiftError {
    errors::mcp::parameter_invalid()
        .tool("nodes")
        .field("scope")
        .accepted("a local project scope beside path and position")
        .error()
}

/// `request` with its selector dispatched to `backend`'s own `local` owner.
pub(in crate::proxy) fn dispatch_nodes_request(
    request: &CallToolRequestParams,
    backend: &str,
) -> Result<CallToolRequestParams, RiftError> {
    let selected: NodesToolParams = serde_json::from_value(
        request.arguments.clone().unwrap_or_default().into(),
    )
    .map_err(|source| {
        errors::mcp::parameter_invalid()
            .tool("nodes")
            .field("arguments")
            .accepted(source.to_string())
            .error()
    })?;
    let (mut params, _) = selected.split();
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

fn qualified_nodes_response(
    value: serde_json::Value,
    backend: &str,
) -> Result<Result<CallToolResponse, rmcp::ErrorData>, RiftError> {
    let mut result: rift_protocol::read::NodesResult = serde_json::from_value(value)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    super::super::identity::qualify_nodes(&mut result, backend)?;
    bounded_response(crate::output::Json(result).into_call_tool_result())
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
