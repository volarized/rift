//! Exact global reads retain the source selection already held by a query response.

use std::sync::Mutex;

use rift_protocol::identity::{SymbolIdentity, SymbolOwner, parse_source_unit_identity};
use rift_protocol::source_read::{GetSourceParams, GetSourceResult};
use rift_protocol::symbol_read::{
    CapturedView, CapturedViewId, GetSymbolParams, GetSymbolResult, SymbolUnavailableReason,
};

use super::{QueryGlobalSelection, RiftMcp, SavedSymbol, read_failure, retained};
use crate::global::candidates::SavedCandidateBackend;
use crate::output::ToolFailure;

pub(super) struct SelectedGlobal {
    pub(super) source: QueryGlobalSelection,
    pub(super) root: SavedSymbol,
    pub(super) facts: Option<std::sync::Arc<rift_search::query::QueryFacts>>,
}

impl RiftMcp {
    fn query_global_selection(
        &self,
        view: &CapturedViewId,
        owner: &SymbolOwner,
        operation: &str,
    ) -> Result<Option<SelectedGlobal>, ToolFailure> {
        if !self.has_captured_context(view) {
            return Ok(None);
        }
        let (saved, material) = match retained(&self.candidate_responses, &self.local_views, view)
            .map_err(read_failure)?
        {
            Ok(selected) => selected,
            Err(SymbolUnavailableReason::ViewContext) => return Ok(None),
            Err(reason) => {
                return Err(super::super::captured_read_refusal(operation, view, reason));
            }
        };
        let selected = saved
            .upstream
            .iter()
            .find(|selected| selected.owners.contains(owner))
            .cloned();
        Ok(selected.map(|source| SelectedGlobal {
            source,
            root: saved.context,
            facts: material.package_facts(),
        }))
    }

    pub(in crate::server) async fn query_global_symbol(
        &self,
        params: &GetSymbolParams,
        context: Option<&Mutex<Option<SavedCandidateBackend>>>,
    ) -> Result<Option<GetSymbolResult>, ToolFailure> {
        let Some(view) = &params.view else {
            return Ok(None);
        };
        let owner = SymbolIdentity::parse(params.id.as_str())
            .map_err(read_failure)?
            .owner()
            .clone();
        let Some(held) = self.query_global_selection(view, &owner, "get_symbol")? else {
            return Ok(None);
        };
        let deadline = self.request_deadline().await;
        self.query_global_symbol_selected(params, held, context, deadline)
            .await
    }

    pub(super) async fn query_global_symbol_selected(
        &self,
        params: &GetSymbolParams,
        held: SelectedGlobal,
        context: Option<&Mutex<Option<SavedCandidateBackend>>>,
        deadline: crate::server::RequestDeadline,
    ) -> Result<Option<GetSymbolResult>, ToolFailure> {
        if captured_symbol(&held, params) {
            return Ok(None);
        }
        let SelectedGlobal {
            source: selected,
            root,
            ..
        } = held;
        if !self.query_global_configuration(&selected).await? {
            return Ok(Some(GetSymbolResult::Unavailable {
                id: params.id.clone(),
                view: Some(root.view),
                reason: SymbolUnavailableReason::ViewContext,
                warnings: Vec::new(),
            }));
        }
        let mut request = params.clone();
        request.view = Some(selected.view.id.clone());
        let mut result = tokio::time::timeout_at(
            deadline.at(),
            self.global.exact_symbol(&selected.configuration, &request),
        )
        .await
        .map_err(|_| {
            read_failure(super::super::super::query::acquisition_failure(
                "retained global symbol exceeded the request deadline",
            ))
        })?
        .map_err(read_failure)?;
        match &mut result {
            GetSymbolResult::Found { view, .. } | GetSymbolResult::Missing { view, .. } => {
                if !same_view(view, &selected.view) {
                    return Err(context_failure(&root.view, "get_symbol"));
                }
                *view = root.view.clone();
            }
            GetSymbolResult::Unavailable { view, .. } => {
                if view
                    .as_ref()
                    .is_some_and(|view| !same_view(view, &selected.view))
                {
                    return Err(context_failure(&root.view, "get_symbol"));
                }
                *view = Some(root.view.clone());
            }
        }
        publish_context(context, &root)?;
        Ok(Some(result))
    }

    pub(in crate::server) async fn query_global_source(
        &self,
        params: &GetSourceParams,
        context: Option<&Mutex<Option<SavedCandidateBackend>>>,
    ) -> Result<Option<GetSourceResult>, ToolFailure> {
        let Some(view) = &params.view else {
            return Ok(None);
        };
        let (owner, _) = parse_source_unit_identity(params.unit.as_str()).map_err(read_failure)?;
        let Some(owner) = owner else {
            return Ok(None);
        };
        let Some(held) = self.query_global_selection(view, &owner, "get_source")? else {
            return Ok(None);
        };
        let deadline = self.request_deadline().await;
        self.query_global_source_selected(params, held, None, context, deadline)
            .await
    }

    pub(super) async fn query_global_source_selected(
        &self,
        params: &GetSourceParams,
        held: SelectedGlobal,
        expected_digest: Option<rift_protocol::identity::SourceDigest>,
        context: Option<&Mutex<Option<SavedCandidateBackend>>>,
        deadline: crate::server::RequestDeadline,
    ) -> Result<Option<GetSourceResult>, ToolFailure> {
        if held.facts.as_ref().is_some_and(|facts| {
            facts
                .sources
                .iter()
                .any(|source| source.unit == params.unit && source.text.is_some())
        }) {
            return Ok(None);
        }
        let digest = expected_digest.or_else(|| {
            held.facts.as_ref().and_then(|facts| {
                facts
                    .sources
                    .iter()
                    .find(|source| source.unit == params.unit)
                    .and_then(|source| source.digest.clone())
            })
        });
        let SelectedGlobal {
            source: selected,
            root,
            ..
        } = held;
        if !self.query_global_configuration(&selected).await? {
            return Ok(Some(GetSourceResult::Unavailable {
                unit: params.unit.clone(),
                view: Some(root.view),
                reason: SymbolUnavailableReason::ViewContext,
                warnings: Vec::new(),
            }));
        }
        let mut request = params.clone();
        request.view = Some(selected.view.id.clone());
        let mut result = tokio::time::timeout_at(
            deadline.at(),
            self.global.exact_source(&selected.configuration, &request),
        )
        .await
        .map_err(|_| {
            read_failure(super::super::super::query::acquisition_failure(
                "retained global source exceeded the request deadline",
            ))
        })?
        .map_err(read_failure)?;
        if params.range.is_none()
            && let Some(digest) = digest
            && let GetSourceResult::Found {
                source,
                source_complete: true,
                ..
            } = &result
            && digest.as_str()
                != data_encoding::HEXLOWER
                    .encode(rift_core::FileDigest::of(source.text.as_bytes()).as_bytes())
        {
            return Ok(Some(GetSourceResult::Unavailable {
                unit: params.unit.clone(),
                view: Some(root.view),
                reason: SymbolUnavailableReason::CorruptPublication,
                warnings: Vec::new(),
            }));
        }
        match &mut result {
            GetSourceResult::Found { view, .. } | GetSourceResult::Missing { view, .. } => {
                if !same_view(view, &selected.view) {
                    return Err(context_failure(&root.view, "get_source"));
                }
                *view = root.view.clone();
            }
            GetSourceResult::Unavailable { view, .. } => {
                if view
                    .as_ref()
                    .is_some_and(|view| !same_view(view, &selected.view))
                {
                    return Err(context_failure(&root.view, "get_source"));
                }
                *view = Some(root.view.clone());
            }
        }
        publish_context(context, &root)?;
        Ok(Some(result))
    }

    async fn query_global_configuration(
        &self,
        selected: &QueryGlobalSelection,
    ) -> Result<bool, ToolFailure> {
        let published = std::sync::Arc::clone(&self.published.read().await.current);
        published
            .configuration
            .accepted_for_tool(super::super::super::wire::ErrorPhase::Read)?;
        Ok(published.configuration.global_configuration() == selected.configuration)
    }
}

fn same_view(actual: &CapturedView, selected: &CapturedView) -> bool {
    actual.id == selected.id
        && crate::proxy::captured_expiry(actual)
            .ok()
            .zip(crate::proxy::captured_expiry(selected).ok())
            .is_some_and(|(actual, selected)| actual == selected)
}

fn captured_symbol(held: &SelectedGlobal, params: &GetSymbolParams) -> bool {
    if params.include.iter().any(|projection| {
        matches!(
            projection,
            rift_protocol::read::GetSymbolInclude::History
                | rift_protocol::read::GetSymbolInclude::Documentation
        )
    }) {
        return false;
    }
    held.facts.as_ref().is_some_and(|facts| {
        facts.symbols.iter().any(|symbol| {
            symbol.symbol.id.as_ref() == Some(&params.id)
                && (!params
                    .include
                    .contains(&rift_protocol::read::GetSymbolInclude::Source)
                    || symbol.declarations.iter().all(|declaration| {
                        declaration.unit.as_ref().is_some_and(|unit| {
                            facts
                                .sources
                                .iter()
                                .any(|source| &source.unit == unit && source.text.is_some())
                        })
                    }))
        })
    })
}

fn context_failure(view: &CapturedView, operation: &str) -> ToolFailure {
    super::super::captured_read_refusal(operation, &view.id, SymbolUnavailableReason::ViewContext)
}

fn publish_context(
    context: Option<&Mutex<Option<SavedCandidateBackend>>>,
    root: &SavedSymbol,
) -> Result<(), ToolFailure> {
    if let Some(context) = context {
        let selected = SavedCandidateBackend {
            backend: "local".to_owned(),
            root: root.root.clone(),
            owner: None,
            view: root.view.clone(),
            project: None,
            upstream_view: None,
        };
        crate::proxy::encode_bounded(&selected, super::super::BYTES_MAX)
            .map_err(super::super::transport_failure)?;
        *context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(selected);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CapturedView, CapturedViewId, same_view};

    #[test]
    fn retained_global_views_preserve_original_expiry_instants() {
        let view = |key: char, expires_at: &str| CapturedView {
            id: CapturedViewId::parse(&key.to_string().repeat(64)).expect("fixture view"),
            expires_at: expires_at.to_owned(),
        };
        let selected = view('a', "2099-10-10T10:05:00.123456789+00:00");
        for expiry in [
            "2099-10-10T10:05:00.123456789Z",
            "2099-10-10T11:05:00.123456789+01:00",
        ] {
            assert!(same_view(&view('a', expiry), &selected));
        }
        for actual in [
            view('b', &selected.expires_at),
            view('a', "2099-10-10T10:05:00.123456788Z"),
            view('a', "2099-10-10T10:05:00.123456790Z"),
            view('a', "invalid"),
        ] {
            assert!(!same_view(&actual, &selected));
        }
        assert!(!same_view(&view('a', "invalid"), &view('a', "invalid")));
    }
}
