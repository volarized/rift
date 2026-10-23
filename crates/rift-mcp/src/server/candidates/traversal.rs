use super::{BYTES_MAX, RiftMcp, SavedRead, read_failure, transport_failure, unavailable};
use rift_error::errors;
use rift_protocol::identity::SymbolOwner;
use rift_protocol::read::{ReadWarning, SearchHit, SearchInclude, SearchParams, SearchResult};
use rift_protocol::search_read::{SearchReadResult, SearchUnavailable};
use rift_protocol::symbol_read::{CapturedView, SymbolUnavailableReason};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SavedTraversal {
    pub(super) root: String,
    pub(super) owner: SymbolOwner,
    pub(super) view: CapturedView,
    params: SearchParams,
    results: Vec<SearchHit>,
    warnings: Vec<ReadWarning>,
    global_backend: Option<crate::global::candidates::SavedCandidateBackend>,
}

impl RiftMcp {
    pub(in crate::server) async fn captured_traversal(
        &self,
        params: &SearchParams,
        context: Option<
            &std::sync::Mutex<Option<crate::global::candidates::SavedCandidateBackend>>,
        >,
    ) -> Result<SearchReadResult, super::ToolFailure> {
        let views = self
            .local_views
            .get()
            .ok_or_else(|| unavailable("retained read material is unavailable"))?;
        let mut saved = match self.saved_traversal(params, views).await? {
            Ok(saved) => saved,
            Err(result) => return Ok(result),
        };
        let accepted = views
            .captured_view(
                saved.view.id.as_str(),
                std::path::Path::new(&saved.root),
                &saved.owner,
            )
            .map_err(read_failure)?;
        match accepted {
            Ok(view) if view == saved.view => {}
            Ok(_) => {
                return Ok(unavailable_result(
                    SymbolUnavailableReason::ViewContext,
                    Some(saved.view),
                ));
            }
            Err(reason) => return Ok(unavailable_result(reason, Some(saved.view))),
        }
        if let Some(context) = context {
            context
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone_from(&saved.global_backend);
        }
        let limit = usize::try_from(
            params
                .limit
                .unwrap_or(rift_core::constants::SEARCH_RESULTS_DEFAULT as u64),
        )
        .map_err(|_| {
            read_failure(
                errors::server::read_invalid()
                    .field("limit")
                    .violation("search limit exceeds the platform bound")
                    .error(),
            )
        })?;
        let (mut results, pagination) = crate::global::page_window(
            std::mem::take(&mut saved.results),
            params.page_index,
            limit,
        );
        if params
            .include
            .as_deref()
            .unwrap_or_default()
            .contains(&SearchInclude::Source)
        {
            match views
                .populate_sources(
                    &mut results,
                    &saved.view.id,
                    std::path::Path::new(&saved.root),
                    &saved.owner,
                )
                .map_err(read_failure)?
            {
                Ok(warnings) => saved.warnings.extend(warnings),
                Err(reason) => return Ok(unavailable_result(reason, Some(saved.view))),
            }
        }
        let result = SearchResult {
            view: Some(saved.view),
            results,
            pagination,
            warnings: saved.warnings,
        };
        crate::proxy::encode_bounded(&result, BYTES_MAX).map_err(transport_failure)?;
        Ok(SearchReadResult::Results(Box::new(result)))
    }
    async fn saved_traversal(
        &self,
        params: &SearchParams,
        views: &rift_server::LocalSymbolViews,
    ) -> Result<Result<SavedTraversal, SearchReadResult>, super::ToolFailure> {
        if let Some(view) = &params.view {
            let rows = self
                .candidate_responses
                .bytes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(row) = rows.iter().find(|row| row.id == view.as_str()) else {
                return Ok(Err(unavailable_result(
                    SymbolUnavailableReason::ViewContext,
                    None,
                )));
            };
            let saved: SavedRead = serde_json::from_slice(&row.bytes).map_err(transport_failure)?;
            let SavedRead::Traversal(saved) = saved else {
                return Ok(Err(unavailable_result(
                    SymbolUnavailableReason::InsufficientCoverage,
                    None,
                )));
            };
            if row.expires <= SystemTime::now() {
                return Ok(Err(unavailable_result(
                    SymbolUnavailableReason::ViewExpired,
                    Some(saved.view),
                )));
            }
            if crate::proxy::normalized_params(&saved.params)
                != crate::proxy::normalized_params(params)
            {
                return Ok(Err(unavailable_result(
                    SymbolUnavailableReason::ViewContext,
                    Some(saved.view),
                )));
            }
            Ok(Ok(*saved))
        } else {
            self.initial_traversal(params, views).await
        }
    }

    async fn initial_traversal(
        &self,
        params: &SearchParams,
        views: &rift_server::LocalSymbolViews,
    ) -> Result<Result<SavedTraversal, SearchReadResult>, super::ToolFailure> {
        let (resolved, answer, warnings, references, _) =
            self.current_search_inputs(params).await?;
        let global_backend = references.package_views().next().map(|view| {
            crate::global::candidates::SavedCandidateBackend {
                backend: "global".to_owned(),
                root: resolved
                    .published
                    .configuration
                    .global_configuration()
                    .endpoint,
                owner: None,
                view: view.clone(),
                project: None,
                upstream_view: None,
            }
        });
        let ready = resolved.published.preparation.is_none();
        let requested = params.clone();
        let ((mut results, mut captured_warnings), material) = self
            .current_captured_read(&resolved, true, move |reads, external_warnings| {
                let (results, mut warnings) =
                    reads.search_traversal_hits(&requested, &answer, &references)?;
                warnings.extend(external_warnings.iter().cloned());
                let traversal = requested.traversal.as_ref().ok_or_else(|| {
                    errors::server::read_invalid()
                        .field("traversal")
                        .violation("a traversal starts at seed")
                        .error()
                })?;
                let material = reads
                    .local_traversal_view_input(traversal, references, ready)?
                    .with_warnings(external_warnings)?;
                Ok(((results, warnings), material))
            })
            .await?
            .0;
        captured_warnings.extend(warnings);
        let root = material
            .root()
            .to_str()
            .ok_or_else(|| unavailable("project root is not UTF-8"))?
            .to_owned();
        let owner = material.owner().clone();
        let mut saved = None;
        views
            .capture_with(material, |view| {
                for hit in &mut results {
                    if hit.view.is_none() {
                        hit.view = Some(view.clone());
                    }
                }
                let response = SavedTraversal {
                    root,
                    owner,
                    view: view.clone(),
                    params: crate::proxy::normalized_params(params),
                    results,
                    warnings: captured_warnings,
                    global_backend,
                };
                let bytes = crate::proxy::encode_bounded(
                    &SavedRead::Traversal(Box::new(response)),
                    BYTES_MAX,
                )
                .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
                let decoded: SavedRead = serde_json::from_slice(&bytes)
                    .map_err(|source| errors::mcp::forward_failed().source(source).error())?;
                let SavedRead::Traversal(response) = decoded else {
                    return errors::server::read_invalid()
                        .field("view")
                        .violation("captured traversal context does not match")
                        .fail();
                };
                self.candidate_responses.save_bytes(
                    view.id.as_str().to_owned(),
                    crate::proxy::captured_expiry(view)?,
                    bytes,
                )?;
                saved = Some(*response);
                Ok(true)
            })
            .map_err(read_failure)?;
        let Some(saved) = saved else {
            return Ok(Err(unavailable_result(
                if ready {
                    SymbolUnavailableReason::InsufficientCoverage
                } else {
                    SymbolUnavailableReason::IndexPreparing
                },
                None,
            )));
        };
        Ok(Ok(saved))
    }
}

fn unavailable_result(
    reason: SymbolUnavailableReason,
    view: Option<CapturedView>,
) -> SearchReadResult {
    SearchReadResult::Unavailable(SearchUnavailable::Unavailable {
        reason,
        view,
        warnings: Vec::new(),
    })
}
