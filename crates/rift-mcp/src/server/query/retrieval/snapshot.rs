//! Local search pages and their readback material share one original publication.

use std::sync::Arc;

use rift_protocol::configuration::WorkspaceConfiguration;
use rift_protocol::read::{ReadWarning, SearchParams, SearchResult, SearchScope};
use rift_protocol::symbol_read::CapturedView;

use super::super::{QueryBudget, acquisition_failure};
use crate::server::{RiftMcp, ToolFailure};

#[cfg(test)]
mod tests;

pub(super) struct Snapshot {
    reads: Arc<rift_server::ReadService>,
    prepared: bool,
    answer: Arc<rift_server::StoreAnswer>,
    references: Arc<rift_server::EngineReferences>,
    warnings: Vec<ReadWarning>,
    configuration: WorkspaceConfiguration,
    view: Option<CapturedView>,
    sources: Option<Arc<rift_server::SearchSnapshot>>,
}

struct Page {
    result: SearchResult,
    contributions: Vec<Vec<rift_core::ContributionKey>>,
    material: Option<rift_server::LocalSymbolViewInput>,
    sources: Arc<rift_server::SearchSnapshot>,
}

impl RiftMcp {
    pub(super) async fn retrieval_snapshot(
        &self,
        request: &SearchParams,
        budget: &QueryBudget,
        deadline: crate::server::RequestDeadline,
    ) -> Result<Option<Snapshot>, ToolFailure> {
        if request.view.is_some()
            || request.change.is_some()
            || request.target == rift_protocol::read::SearchParamsTarget::Commit
            || request.scope != SearchScope::Local
        {
            return Ok(None);
        }
        if let Some(rev) = &request.rev {
            return self
                .revision_retrieval_snapshot(rev.clone(), budget, deadline)
                .await
                .map(Some);
        }
        let (resolved, answer, warnings, references, _) =
            self.current_search_inputs(request).await?;
        let configuration = resolved
            .published
            .configuration
            .accepted_for_tool(rift_protocol::error::ErrorPhase::Read)?;
        let prepared = resolved.published.preparation.is_none();
        let reads = Arc::clone(&resolved.published.reads);
        let mut captured_warnings = Self::current_tree_warnings(&resolved, true);
        captured_warnings.extend(warnings);
        Ok(Some(Snapshot {
            reads,
            prepared,
            answer: Arc::new(answer),
            references: Arc::new(references),
            warnings: captured_warnings,
            configuration,
            view: None,
            sources: None,
        }))
    }

    async fn revision_retrieval_snapshot(
        &self,
        revision: rift_protocol::read::RevisionId,
        budget: &QueryBudget,
        deadline: crate::server::RequestDeadline,
    ) -> Result<Snapshot, ToolFailure> {
        let resolved = self
            .published_workspace(rift_protocol::error::ErrorPhase::Read, deadline)
            .await?;
        let configuration = resolved
            .published
            .configuration
            .accepted_for_tool(rift_protocol::error::ErrorPhase::Read)?;
        let capture = self.revision_read_factory(&resolved.published, revision)?;
        let reads = self
            .query_work(budget, deadline, move |_| capture())
            .await?;
        Ok(Snapshot {
            reads,
            prepared: true,
            answer: Arc::new(rift_server::StoreAnswer::identifier_only()),
            references: Arc::default(),
            warnings: Vec::new(),
            configuration,
            view: None,
            sources: None,
        })
    }

    pub(super) async fn retrieval_snapshot_page(
        &self,
        snapshot: &mut Snapshot,
        request: SearchParams,
        budget: &QueryBudget,
        deadline: crate::server::RequestDeadline,
    ) -> Result<(Box<SearchResult>, Vec<Vec<rift_core::ContributionKey>>), ToolFailure> {
        let first = snapshot.view.is_none();
        let prepared = snapshot.prepared;
        let answer = Arc::clone(&snapshot.answer);
        let references = Arc::clone(&snapshot.references);
        let configuration = snapshot.configuration.clone();
        let reads = Arc::clone(&snapshot.reads);
        let sources = snapshot.sources.as_ref().map(Arc::clone);
        let mut page = self
            .query_work(budget, deadline, move |budget| {
                let mut selected = request;
                selected.view = None;
                let page_index = selected.page_index;
                let sources = if let Some(sources) = sources {
                    sources
                } else {
                    budget.charge(&selected)?;
                    Arc::new(reads.capture_search(selected, answer, references)?)
                };
                let (result, contributions) = sources.page(page_index, budget)?;
                let material = if first {
                    let material = sources.view_input(prepared)?;
                    let selection = material.provider_selection(&configuration)?;
                    Some(material.with_provider_selection(selection)?)
                } else {
                    None
                };
                Ok(Page {
                    result,
                    contributions,
                    material,
                    sources,
                })
            })
            .await?;
        snapshot.sources = Some(Arc::clone(&page.sources));
        page.result
            .warnings
            .extend(snapshot.warnings.iter().cloned());
        let views = Arc::clone(&self.local_views);
        let responses = Arc::clone(&self.candidate_responses);
        let original_view = snapshot.view.clone();
        let (result, view) = self
            .query_work(budget, deadline, move |budget| {
                budget.charge(&page.result)?;
                let view = if let Some(view) = original_view {
                    view
                } else {
                    let material = page.material.take().ok_or_else(|| {
                        acquisition_failure("producer publication is unavailable")
                    })?;
                    let root = material.root().to_path_buf();
                    let owner = material.owner().clone();
                    let views = views.get().ok_or_else(|| {
                        acquisition_failure("producer publication is unavailable")
                    })?;
                    views
                        .capture_with(material, |view| responses.save_symbol(&root, &owner, view))?
                        .ok_or_else(|| {
                            acquisition_failure("producer publication could not be retained")
                        })?
                };
                page.result.view = Some(view.clone());
                for hit in &mut page.result.results {
                    if hit.view.is_none() {
                        hit.view = Some(view.clone());
                    }
                }
                budget.charge(&page.result)?;
                Ok(((Box::new(page.result), page.contributions), view))
            })
            .await?;
        snapshot.view = Some(view);
        Ok(result)
    }
}
