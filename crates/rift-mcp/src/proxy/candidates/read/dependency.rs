use super::{CallToolRequestParams, GetSymbolResult, RiftError, SavedCandidateBackend, invalid};

pub(in crate::proxy) fn upstream_backend(
    selected: &SavedCandidateBackend,
) -> SavedCandidateBackend {
    let mut upstream = selected.clone();
    if let Some(view) = &selected.upstream_view {
        upstream.view = view.clone();
        if upstream.backend == "global" {
            upstream.owner = None;
        }
        upstream.project = None;
        upstream.upstream_view = None;
    }
    upstream
}

pub(super) fn upstream_request(
    mut request: CallToolRequestParams,
    selected: &SavedCandidateBackend,
) -> Result<CallToolRequestParams, RiftError> {
    if selected.upstream_view.is_none() {
        return Ok(request);
    }
    let upstream = upstream_backend(selected);
    request
        .arguments
        .as_mut()
        .ok_or_else(|| invalid("arguments", "captured request has no arguments"))?
        .insert("view".to_owned(), serde_json::json!(upstream.view.id));
    if upstream.backend != "global" {
        return Ok(request);
    }
    Ok(with_backend_context(request, &upstream))
}

pub(in crate::proxy) fn with_backend_context(
    mut request: CallToolRequestParams,
    selected: &SavedCandidateBackend,
) -> CallToolRequestParams {
    let meta = request
        .meta
        .get_or_insert_with(rmcp::model::RequestMetaObject::new);
    for (field, value) in [
        ("backend", serde_json::json!(selected.backend)),
        ("root", serde_json::json!(selected.root)),
        ("owner", serde_json::json!(selected.owner)),
        ("view", serde_json::json!(selected.view)),
    ] {
        meta.0.insert(field.to_owned(), value);
    }
    request
}

pub(super) fn public_symbol_view(result: &mut GetSymbolResult, selected: &SavedCandidateBackend) {
    if selected.upstream_view.is_none() {
        return;
    }
    match result {
        GetSymbolResult::Found { view, .. } | GetSymbolResult::Missing { view, .. } => {
            *view = selected.view.clone();
        }
        GetSymbolResult::Unavailable {
            view: Some(view), ..
        } => *view = selected.view.clone(),
        GetSymbolResult::Unavailable { view: None, .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_protocol::symbol_read::{CapturedView, CapturedViewId};
    use serde_json::json;

    #[test]
    fn recorded_backend_context_preserves_the_original_global_view()
    -> Result<(), Box<dyn std::error::Error>> {
        let remote = CapturedView {
            id: CapturedViewId::parse("claims.remote")?,
            expires_at: "2030-01-01T00:00:00Z".to_owned(),
        };
        let selected = SavedCandidateBackend {
            backend: "global".to_owned(),
            root: "https://rift.test".to_owned(),
            owner: Some(rift_protocol::identity::SymbolOwner::Local),
            view: CapturedView {
                id: CapturedViewId::parse("claims.public")?,
                expires_at: "2030-01-01T00:00:00Z".to_owned(),
            },
            project: Some("local".to_owned()),
            upstream_view: Some(remote.clone()),
        };
        let normalized = upstream_backend(&selected);
        assert_eq!(normalized.view, remote);
        assert!(normalized.owner.is_none());
        assert!(normalized.project.is_none());
        assert!(normalized.upstream_view.is_none());
        let request = CallToolRequestParams::new("get_symbol").with_arguments(
            serde_json::from_value(json!({
                "id":"rift://symbol/cargo/crates.io/demo@1.0.0/rust/demo/beacon",
                "view":selected.view.id,
            }))?,
        );
        let request = upstream_request(request, &selected)?;
        assert_eq!(
            request.arguments.as_ref().ok_or("arguments")?["view"],
            remote.id.as_str()
        );
        let meta = request.meta.ok_or("recorded backend context")?;
        assert_eq!(meta.0["backend"], "global");
        assert_eq!(meta.0["root"], "https://rift.test");
        assert!(meta.0["owner"].is_null());
        assert_eq!(meta.0["view"], serde_json::to_value(remote)?);
        Ok(())
    }
}
