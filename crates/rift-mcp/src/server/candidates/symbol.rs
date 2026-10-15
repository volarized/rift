use super::{
    BYTES_MAX, CandidateResponses, RiftMcp, SavedResponse, read_failure, transport_failure,
};
use crate::output::Json;
use rift_error::{RiftError, errors};
use rift_protocol::identity::{SymbolIdentity, SymbolOwner};
use rift_protocol::symbol_read::{
    CapturedView, CapturedViewId, GetSymbolParams, GetSymbolResult, SymbolUnavailableReason,
};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc, time::SystemTime};

#[derive(Deserialize, Serialize)]
#[serde(tag = "operation", content = "response", rename_all = "snake_case")]
pub(super) enum SavedRead {
    Search(Box<SavedResponse>),
    Symbol(SavedSymbol),
    Traversal(Box<super::traversal::SavedTraversal>),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SavedSymbol {
    pub(super) root: String,
    pub(super) owner: SymbolOwner,
    pub(super) view: CapturedView,
}

impl CandidateResponses {
    pub(super) fn save_symbol(
        &self,
        root: &Path,
        owner: &SymbolOwner,
        view: &CapturedView,
    ) -> Result<bool, RiftError> {
        let root = root.to_str().ok_or_else(|| {
            errors::server::read_invalid()
                .field("root")
                .violation("project root is not UTF-8")
                .error()
        })?;
        let encoded = crate::proxy::encode_bounded(
            &SavedRead::Symbol(SavedSymbol {
                root: root.to_owned(),
                owner: owner.clone(),
                view: view.clone(),
            }),
            BYTES_MAX,
        )
        .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
        self.save_bytes(
            view.id.as_str().to_owned(),
            crate::proxy::captured_expiry(view)?,
            encoded,
        )?;
        Ok(true)
    }
}

impl RiftMcp {
    pub(super) fn has_captured_context(&self, view: &CapturedViewId) -> bool {
        self.candidate_responses
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|row| row.id == view.as_str())
    }

    pub(super) fn captured_context(
        &self,
        view: &CapturedViewId,
        operation: &str,
    ) -> Result<SavedSymbol, super::ToolFailure> {
        let rows = self
            .candidate_responses
            .bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let row = rows
            .iter()
            .find(|row| row.id == view.as_str() && row.expires > SystemTime::now())
            .ok_or_else(|| {
                super::captured_read_refusal(operation, view, SymbolUnavailableReason::ViewExpired)
            })?;
        let saved: SavedRead = serde_json::from_slice(&row.bytes).map_err(transport_failure)?;
        let saved = match saved {
            SavedRead::Search(saved) => SavedSymbol {
                root: saved.response.root,
                owner: saved.response.owner,
                view: saved.response.view,
            },
            SavedRead::Symbol(saved) => saved,
            SavedRead::Traversal(saved) => SavedSymbol {
                root: saved.root,
                owner: saved.owner,
                view: saved.view,
            },
        };
        if saved.view.id != *view {
            return Err(super::captured_read_refusal(
                operation,
                view,
                SymbolUnavailableReason::ViewContext,
            ));
        }
        Ok(saved)
    }

    #[cfg(test)]
    pub(in crate::server) async fn exact_symbol(
        &self,
        params: GetSymbolParams,
    ) -> Result<Json<GetSymbolResult>, super::ToolFailure> {
        self.exact_symbol_with_context(params, None).await
    }

    pub(in crate::server) async fn exact_symbol_with_context(
        &self,
        params: GetSymbolParams,
        context: Option<
            &std::sync::Mutex<Option<crate::global::candidates::SavedCandidateBackend>>,
        >,
    ) -> Result<Json<GetSymbolResult>, super::ToolFailure> {
        let owner = SymbolIdentity::parse(params.id.as_str())
            .map_err(read_failure)?
            .owner()
            .clone();
        if let Some(view) = params.view.as_ref()
            && (self.has_captured_context(view)
                || !matches!(
                    owner,
                    SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
                ))
        {
            let context = self.captured_context(view, "get_symbol")?;
            let views = self.local_views.get().ok_or_else(|| {
                super::captured_read_refusal(
                    "get_symbol",
                    view,
                    SymbolUnavailableReason::ViewExpired,
                )
            })?;
            return views
                .resolve(&params, Path::new(&context.root), &context.owner)
                .map(Json)
                .map_err(read_failure);
        }
        if matches!(
            owner,
            SymbolOwner::Package { .. } | SymbolOwner::Runtime { .. }
        ) {
            return self.global_symbol(&params, context).await;
        }
        let rev = params.rev.clone();
        let server = self.clone();
        let operation =
            move |reads: &rift_server::ReadService,
                  ready: bool,
                  warnings: Vec<rift_protocol::read::ReadWarning>| {
                let mut selected = params.clone();
                selected.rev = None;
                let input = reads
                    .local_symbol_view_input(&selected, ready)?
                    .with_warnings(warnings.clone())?;
                let root = input.root().to_path_buf();
                let owner = input.owner().clone();
                let held_warnings = input.warnings()?;
                let views = server.local_views.get().ok_or_else(|| {
                    errors::server::read_unavailable()
                        .operation("get_symbol")
                        .detail("captured selection is unavailable")
                        .error()
                })?;
                let cache = Arc::clone(&server.candidate_responses);
                let view =
                    views.capture_with(input, |view| cache.save_symbol(&root, &owner, view))?;
                let Some(view) = view else {
                    return Ok(GetSymbolResult::Unavailable {
                        id: selected.id,
                        view: None,
                        reason: SymbolUnavailableReason::IndexPreparing,
                        warnings: held_warnings,
                    });
                };
                selected.view = Some(view.id);
                views.resolve(&selected, &root, &owner)
            };
        if rev.is_some() {
            return self
                .read_at(rev, move |reads| operation(reads, true, Vec::new()))
                .await;
        }
        let deadline = self.request_deadline().await;
        let resolved = self
            .published_workspace(super::wire::ErrorPhase::Read, deadline)
            .await?;
        let ready = resolved.published.preparation.is_none();
        self.current_captured_read(&resolved, true, move |reads, warnings| {
            operation(reads, ready, warnings)
        })
        .await
    }
    async fn global_symbol(
        &self,
        params: &GetSymbolParams,
        context: Option<
            &std::sync::Mutex<Option<crate::global::candidates::SavedCandidateBackend>>,
        >,
    ) -> Result<Json<GetSymbolResult>, super::ToolFailure> {
        let deadline = self.request_deadline().await;
        let resolved = self
            .published_workspace(super::wire::ErrorPhase::Read, deadline)
            .await?;
        resolved
            .published
            .configuration
            .accepted_for_tool(super::wire::ErrorPhase::Read)?;
        let configuration = resolved.published.configuration.global_configuration();
        let result = tokio::time::timeout_at(
            deadline.at(),
            self.global.exact_symbol(&configuration, params),
        )
        .await
        .map_err(|_| {
            read_failure(
                errors::server::read_unavailable()
                    .operation("get_symbol")
                    .detail("a request spent its whole readiness budget")
                    .error(),
            )
        })?
        .map_err(read_failure)?;
        let view = match &result {
            GetSymbolResult::Found { view, .. } | GetSymbolResult::Missing { view, .. } => {
                Some(view)
            }
            GetSymbolResult::Unavailable { view, .. } => view.as_ref(),
        };
        if let Some(context) = context
            && let Some(view) = view
        {
            let selected = crate::global::candidates::SavedCandidateBackend {
                backend: "global".to_owned(),
                root: configuration.endpoint,
                owner: None,
                view: view.clone(),
            };
            crate::proxy::encode_bounded(&selected, super::BYTES_MAX)
                .map_err(super::transport_failure)?;
            *context
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(selected);
        }
        Ok(Json(result))
    }
}

impl super::ReadAnswer for GetSymbolResult {
    fn warnings_mut(&mut self) -> &mut Vec<super::ReadWarning> {
        match self {
            Self::Found { warnings, .. }
            | Self::Missing { warnings, .. }
            | Self::Unavailable { warnings, .. } => warnings,
        }
    }
}
