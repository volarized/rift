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
    selected: SelectedPaths,
}

impl ReadService {
    /// Captures one source selection before producing any search page.
    ///
    /// # Errors
    /// Returns the existing request, source-selection or search refusal.
    pub fn capture_search(
        self: Arc<Self>,
        params: SearchParams,
        store: Arc<StoreAnswer>,
        references: Arc<EngineReferences>,
    ) -> Result<SearchSnapshot, RiftError> {
        self.validate_dependency_scope(&params.scope, params.rev.as_ref())?;
        self.validated_pattern(&params, &store, &references)?;
        super::accepted_query(&params)?;
        super::search_page_limit(&params)?;
        let selected = self.selected_paths(params.paths.as_ref())?;
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
        budget.charge(&self.params)?;
        let mut params = self.params.clone();
        params.page_index = page_index;
        self.reads.search_selected_with_references(
            &params,
            &self.store,
            &self.references,
            Some(budget),
            Some(&self.selected),
        )
    }

    /// Captures readback material from the same source selection as every page.
    ///
    /// # Errors
    /// Refuses conflicting source owners, files or retained material.
    pub fn view_input(&self, ready: bool) -> Result<LocalSymbolViewInput, RiftError> {
        let input = self.reads.local_search_view_input(ready)?;
        match &self.selected.force_include {
            Some(sources) if self.params.scope != SearchScope::Global => {
                input.with_selected_sources(sources)
            }
            _ => Ok(input),
        }
    }
}
