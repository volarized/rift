use super::{CANDIDATE_BYTES_MAX, CapturedViewId, RiftProxy, SavedCandidateBackend, invalid};
use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::symbol_read::{CapturedView, GetSymbolParams, GetSymbolResult};
use rmcp::handler::server::tool::IntoCallToolResult;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResponse, ClientRequest, ServerResult,
};
use std::time::SystemTime;

mod dependency;
pub(in crate::proxy) use dependency::{upstream_backend, with_backend_context};
#[cfg(test)]
mod unavailable_tests;

impl RiftProxy {
    pub(super) async fn capture_query_read(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        let forwarded = self.query_forward(request).await?;
        if forwarded.owned {
            return Ok(Some(forwarded.response));
        }
        let name = forwarded.backend.as_str();
        let backend = if name == "local" {
            self
        } else {
            self.backends
                .get(name)
                .ok_or_else(|| self.extra.refusal(name, "search", "scope"))?
        };
        let root = accepted_root(backend)?;
        let response = forwarded.response;
        if let Ok(CallToolResponse::Complete(result)) = &response
            && result.is_error != Some(true)
        {
            super::super::encode_bounded(result, CANDIDATE_BYTES_MAX)
                .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
            let value = result.structured_content.as_ref().ok_or_else(|| {
                invalid(
                    "structuredContent",
                    "query response has no structured result",
                )
            })?;
            let decoded: rift_protocol::query::QueryReadResult =
                serde_json::from_value(value.clone())
                    .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
            let view = match decoded {
                rift_protocol::query::QueryReadResult::Results(answer) => answer.view,
                rift_protocol::query::QueryReadResult::Unavailable(
                    rift_protocol::search_read::SearchUnavailable::Unavailable {
                        reason:
                            rift_protocol::symbol_read::SymbolUnavailableReason::ViewExpired
                            | rift_protocol::symbol_read::SymbolUnavailableReason::ViewContext,
                        ..
                    },
                ) => None,
                rift_protocol::query::QueryReadResult::Unavailable(
                    rift_protocol::search_read::SearchUnavailable::Unavailable { view, .. },
                ) => view,
            };
            if let Some(view) = view {
                let selected = SavedCandidateBackend {
                    backend: name.to_owned(),
                    root: root.to_owned(),
                    owner: None,
                    view,
                    project: None,
                    upstream_view: None,
                };
                match self
                    .candidate_views
                    .backend(&selected.view.id, SystemTime::now())?
                    .map_err(|reason| view_refusal(&selected.view.id, reason))?
                {
                    Some(held)
                        if held.backend != selected.backend
                            || held.root != selected.root
                            || held.view != selected.view =>
                    {
                        return Err(invalid("view", "captured backend context does not match"));
                    }
                    Some(_) => {}
                    None => {
                        let mut pending = self.candidate_views.reserve_backend(
                            &selected.backend,
                            &selected.root,
                            None,
                            SystemTime::now(),
                        )?;
                        pending.finalize(&selected, SystemTime::now())?;
                    }
                }
            }
        }
        Ok(Some(response))
    }

    pub(in crate::proxy) async fn captured_read(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<Option<Result<CallToolResponse, rmcp::ErrorData>>, RiftError> {
        if let Some(response) = self.query_material_read(request).await? {
            return Ok(Some(response));
        }
        match request.name.as_ref() {
            "get_symbol" => self.captured_symbol(request).await,
            _ => Ok(None),
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

    pub(in crate::proxy) fn selected_read_backend<'a>(
        &'a self,
        view: &CapturedViewId,
        owner: Option<&SymbolOwner>,
    ) -> Result<(&'a Self, SavedCandidateBackend), RiftError> {
        let selected = self
            .candidate_views
            .backend_for(view, owner, SystemTime::now())?
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

pub(in crate::proxy) fn canonical_symbol_response(result: &rmcp::model::CallToolResult) -> bool {
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

pub(in crate::proxy) fn validate_symbol(
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
    fn source_backend_retains_global_context_and_refuses_changed_endpoint() {
        let params: rift_protocol::source_read::GetSourceParams = serde_json::from_value(
            serde_json::json!({"unit":"rift://source/cargo/crates.io/demo@1.0.0/src/lib.rs"}),
        )
        .expect("source request");
        assert!(params.is_valid());
        assert!(params.view.is_none());
        let (owner, _) = rift_protocol::identity::parse_source_unit_identity(params.unit.as_str())
            .expect("source owner");
        let owner = owner.expect("released owner");
        let view = view();
        let response = response(&view);
        let selected = symbol_backend(&response, &view, "local", "/fixture/main", &owner)
            .expect("fresh global source context");
        assert_eq!(selected.backend, "global");
        assert_eq!(selected.owner, None);
        assert_eq!(selected.root, "/fixture/global");
        assert_eq!(selected.view, view);
        let mut changed = response.clone();
        changed
            .meta
            .as_mut()
            .expect("metadata")
            .insert("root".to_owned(), serde_json::json!("/fixture/foreign"));
        assert!(validate_global_backend(&changed, &selected, Some(&view)).is_err());
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
