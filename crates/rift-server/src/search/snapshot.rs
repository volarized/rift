use std::sync::Arc;

use super::{
    ReadService, RiftError, SearchParams, SearchResult, SearchScope, SelectedPaths, StoreAnswer,
};
use crate::{EngineReferences, LocalSymbolViewInput, QueryFactBudget};

/// One search and its original selected sources, retained across producer pages.
pub struct SearchSnapshot {
    reads: Arc<ReadService>,
    params: SearchParams,
    store: Arc<StoreAnswer>,
    references: Arc<EngineReferences>,
    selected: SearchSelection,
}

enum SearchSelection {
    Sources(Box<SelectedPaths>),
    Commits(SearchResult),
}

impl ReadService {
    /// Captures one source selection before producing any search page.
    ///
    /// # Errors
    /// Returns the existing request, source-selection, history or query admission refusal.
    pub fn capture_search(
        self: Arc<Self>,
        params: SearchParams,
        store: Arc<StoreAnswer>,
        references: Arc<EngineReferences>,
        budget: &mut QueryFactBudget<'_>,
    ) -> Result<SearchSnapshot, RiftError> {
        budget.check()?;
        let selected = if params.target == rift_protocol::read::SearchParamsTarget::Commit {
            SearchSelection::Commits(self.capture_commit_search(&params, budget)?)
        } else {
            self.validate_dependency_scope(&params.scope, params.rev.as_ref())?;
            self.validated_pattern(&params, &store, &references)?;
            super::accepted_query(&params)?;
            super::search_page_limit(&params)?;
            if self.revision().is_some() && super::force_include_requested(&params) {
                return rift_error::errors::server::read_unsupported()
                    .capability("force_include at a revision")
                    .fail();
            }
            let selected = self.selected_paths(params.paths.as_ref())?;
            budget.charge_material(size_of::<SelectedPaths>())?;
            SearchSelection::Sources(Box::new(selected))
        };
        Ok(SearchSnapshot {
            reads: self,
            params,
            store,
            references,
            selected,
        })
    }
}

impl SearchSnapshot {
    /// Answers a page using the original read service and selected source bytes.
    ///
    /// # Errors
    /// Returns the original search or query admission refusal.
    pub fn page(
        &self,
        page_index: u64,
        budget: &mut QueryFactBudget<'_>,
    ) -> Result<(SearchResult, Vec<Vec<rift_core::ContributionKey>>), RiftError> {
        let selected = match &self.selected {
            SearchSelection::Sources(selected) => selected,
            SearchSelection::Commits(held) => return self.commit_page(held, page_index, budget),
        };
        budget.charge(&self.params)?;
        let mut params = self.params.clone();
        params.page_index = page_index;
        self.reads.search_selected_with_references(
            &params,
            &self.store,
            &self.references,
            Some(budget),
            Some(selected),
        )
    }

    fn commit_page(
        &self,
        held: &SearchResult,
        page_index: u64,
        budget: &mut QueryFactBudget<'_>,
    ) -> Result<(SearchResult, Vec<Vec<rift_core::ContributionKey>>), RiftError> {
        let limit = super::search_page_limit(&self.params)?;
        let (window, pagination) = crate::read::page_range(held.results.len(), page_index, limit);
        budget.charge_material(
            window
                .len()
                .checked_mul(
                    size_of::<rift_protocol::read::SearchHit>()
                        + size_of::<Vec<rift_core::ContributionKey>>(),
                )
                .ok_or_else(snapshot_reservation_failure)?,
        )?;
        let mut results = Vec::new();
        let mut contributions = Vec::new();
        results
            .try_reserve_exact(window.len())
            .map_err(|_| snapshot_reservation_failure())?;
        contributions
            .try_reserve_exact(window.len())
            .map_err(|_| snapshot_reservation_failure())?;
        for hit in &held.results[window] {
            budget.check()?;
            budget.charge(hit)?;
            if let rift_protocol::read::SearchHitTarget::Commit { commit } = &hit.hit {
                budget.charge_material(size_of::<rift_protocol::read::CommitHit>())?;
                budget.charge_material(
                    commit
                        .paths
                        .len()
                        .checked_mul(size_of::<rift_protocol::read::ProjectPath>())
                        .ok_or_else(snapshot_reservation_failure)?,
                )?;
            }
            results.push(hit.clone());
            contributions.push(Vec::new());
        }
        budget.charge_material(
            held.warnings
                .len()
                .checked_mul(size_of::<rift_protocol::read::ReadWarning>())
                .ok_or_else(snapshot_reservation_failure)?,
        )?;
        budget.charge(&held.warnings)?;
        Ok((
            SearchResult {
                view: None,
                results,
                pagination,
                warnings: held.warnings.clone(),
            },
            contributions,
        ))
    }

    /// Captures readback material from the same source selection as every page.
    ///
    /// # Errors
    /// Refuses conflicting source owners, files or retained material.
    pub fn view_input(&self, ready: bool) -> Result<LocalSymbolViewInput, RiftError> {
        let input = self.reads.local_search_view_input(ready)?;
        let SearchSelection::Sources(selected) = &self.selected else {
            return Ok(input);
        };
        match &selected.force_include {
            Some(sources) if self.params.scope != SearchScope::Global => {
                input.with_selected_sources(sources)
            }
            _ => Ok(input),
        }
    }
}

fn snapshot_reservation_failure() -> RiftError {
    rift_error::errors::server::read_unavailable()
        .operation("capture commit search")
        .detail("commit page material could not be retained")
        .error()
}
