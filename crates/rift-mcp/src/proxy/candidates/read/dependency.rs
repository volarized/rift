use super::{
    CANDIDATE_BYTES_MAX, CallToolRequestParams, CapturedView, CapturedViewId, GetSymbolResult,
    RiftError, RiftProxy, SavedCandidateBackend, SearchReadResult, SymbolIdentity, SymbolOwner,
    SystemTime, errors, global_backend, invalid, view_refusal,
};
use sha2::{Digest, Sha256};

impl RiftProxy {
    pub(super) fn dependency_views(
        &self,
        result: &rmcp::model::CallToolResult,
        value: serde_json::Value,
        project: &str,
        root: &str,
    ) -> Result<serde_json::Value, RiftError> {
        let mut decoded: SearchReadResult = serde_json::from_value(value)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        if let SearchReadResult::Results(answer) = &mut decoded {
            let top = answer
                .view
                .as_ref()
                .ok_or_else(|| invalid("view", "captured response has no selected view"))?;
            for hit in &mut answer.results {
                let view = hit
                    .view
                    .as_ref()
                    .ok_or_else(|| invalid("view", "captured response has no selected view"))?;
                let owner = match &hit.hit {
                    rift_protocol::search::SearchHitTarget::Symbol { symbol } => symbol
                        .id
                        .as_ref()
                        .map(|id| {
                            SymbolIdentity::parse(id.as_str())
                                .map(|identity| identity.owner().clone())
                        })
                        .transpose()?,
                    _ => None,
                };
                let owner = owner.filter(|owner| {
                    matches!(
                        owner,
                        SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
                    )
                });
                let Some(owner) = owner else {
                    if view == top {
                        continue;
                    }
                    return Err(invalid("view", "captured backend context does not match"));
                };
                let meta = result
                    .meta
                    .as_ref()
                    .ok_or_else(|| invalid("view", "captured backend mapping is unavailable"))?;
                let upstream = global_backend(meta, view)?;
                let selected = dependency_backend(top, project, root, &owner, upstream)?;
                let retained = self
                    .candidate_views
                    .backend(&selected.view.id, SystemTime::now())?
                    .map_err(|reason| view_refusal(&selected.view.id, reason))?;
                if let Some(held) = retained {
                    if held != selected {
                        return Err(invalid("view", "captured backend context does not match"));
                    }
                } else {
                    let mut pending = self
                        .candidate_views
                        .reserve_captured(&selected, SystemTime::now())?;
                    pending.finalize(&selected, SystemTime::now())?;
                }
                hit.view = Some(selected.view);
            }
        }
        serde_json::to_value(decoded)
            .map_err(|source| errors::mcp::forward_failed().source(source).error())
    }
}

fn dependency_backend(
    top: &CapturedView,
    project: &str,
    root: &str,
    owner: &SymbolOwner,
    mut upstream: SavedCandidateBackend,
) -> Result<SavedCandidateBackend, RiftError> {
    let key = (top, project, root, owner, &upstream);
    let encoded = super::super::super::encode_bounded(&key, CANDIDATE_BYTES_MAX)
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
    let id = CapturedViewId::parse(&data_encoding::HEXLOWER.encode(&Sha256::digest(encoded)))?;
    let expires_at =
        if super::super::captured_expiry(top)? <= super::super::captured_expiry(&upstream.view)? {
            top.expires_at.clone()
        } else {
            upstream.view.expires_at.clone()
        };
    upstream.upstream_view = Some(upstream.view.clone());
    upstream.view = CapturedView { id, expires_at };
    upstream.owner = Some(owner.clone());
    upstream.project = Some(project.to_owned());
    Ok(upstream)
}

pub(super) fn upstream_backend(selected: &SavedCandidateBackend) -> SavedCandidateBackend {
    let mut upstream = selected.clone();
    if let Some(view) = &selected.upstream_view {
        upstream.view = view.clone();
        upstream.owner = None;
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
    let meta = request
        .meta
        .get_or_insert_with(rmcp::model::RequestMetaObject::new);
    for (field, value) in [
        ("backend", serde_json::json!(upstream.backend)),
        ("root", serde_json::json!(upstream.root)),
        ("owner", serde_json::json!(upstream.owner)),
        ("view", serde_json::json!(upstream.view)),
    ] {
        meta.0.insert(field.to_owned(), value);
    }
    Ok(request)
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

    #[test]
    fn dependency_views_bind_project_owner_and_original_selection() {
        let top = CapturedView {
            id: CapturedViewId::parse("claims.local").expect("view"),
            expires_at: "2030-01-01T00:10:00Z".to_owned(),
        };
        let remote = SavedCandidateBackend {
            backend: "global".to_owned(),
            root: "https://rift.test".to_owned(),
            owner: None,
            view: CapturedView {
                id: CapturedViewId::parse("claims.remote").expect("view"),
                expires_at: "2030-01-01T00:15:00Z".to_owned(),
            },
            project: None,
            upstream_view: None,
        };
        let owner = SymbolIdentity::parse(
            "rift://symbol/pypi/pypi.org/greeting@1.0.0/python/greeting/core/greet",
        )
        .expect("identity")
        .owner()
        .clone();
        let main =
            dependency_backend(&top, "local", "/main", &owner, remote.clone()).expect("main");
        let docs =
            dependency_backend(&top, "docs", "/main/docs", &owner, remote.clone()).expect("docs");
        assert_ne!(main.view.id, docs.view.id);
        assert_eq!(main.view.expires_at, top.expires_at);
        assert_eq!(upstream_backend(&main), remote);
        assert_eq!(
            dependency_backend(&top, "local", "/main", &owner, upstream_backend(&main))
                .expect("held"),
            main
        );
        let mut earlier = remote;
        earlier.view.expires_at = "2030-01-01T00:05:00Z".to_owned();
        let selected = dependency_backend(&top, "local", "/main", &owner, earlier.clone())
            .expect("earlier cloud expiry");
        assert_eq!(selected.view.expires_at, earlier.view.expires_at);
        assert_eq!(selected.upstream_view.as_ref(), Some(&earlier.view));
        for view in [Some(earlier.view.clone()), None] {
            let mut response: GetSymbolResult = serde_json::from_value(serde_json::json!({
                "outcome":"unavailable",
                "id":"rift://symbol/pypi/pypi.org/greeting@1.0.0/python/greeting/core/greet",
                "view":view, "reason":"insufficient_coverage"
            }))
            .expect("unavailable symbol reply");
            public_symbol_view(&mut response, &selected);
            let GetSymbolResult::Unavailable {
                view: returned,
                reason,
                ..
            } = response
            else {
                panic!("unavailable reply remains unavailable");
            };
            assert_eq!(returned, view.map(|_| selected.view.clone()));
            assert_eq!(
                reason,
                rift_protocol::symbol_read::SymbolUnavailableReason::InsufficientCoverage
            );
        }
    }
}
