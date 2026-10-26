//! Acquired relationships and occurrences retained with their original local publication.

use super::*;

impl LocalSymbolViewInput {
    /// Retains acquired relationships and occurrences without duplicating symbol or source payloads.
    ///
    /// # Errors
    /// Refuses acquisition from a different source revision.
    pub fn with_query_references(
        self,
        references: crate::EngineReferences,
    ) -> Result<Self, RiftError> {
        let references = references.into_provider_references();
        if !references.has_provider_references() {
            return Ok(self);
        }
        let references = match &self.material.query_references {
            Some(existing) => existing.combine_provider_references(&references)?,
            None => references,
        };
        Ok(self.with_query_references_arc(Some(Arc::new(references))))
    }

    /// Acquired relationships and occurrences from this exact original publication.
    #[must_use]
    pub fn query_references(&self) -> Option<&crate::EngineReferences> {
        self.material.query_references.as_deref()
    }

    pub(super) fn with_query_references_arc(
        mut self,
        references: Option<Arc<crate::EngineReferences>>,
    ) -> Self {
        let bytes = |references: &crate::EngineReferences| {
            references
                .retained_material_bytes()
                .saturating_add(2 * std::mem::size_of::<usize>())
        };
        let previous = self.material.query_references.as_deref().map_or(0, bytes);
        self.retained_bytes = self
            .retained_bytes
            .saturating_sub(previous)
            .saturating_add(references.as_deref().map_or(0, bytes));
        self.material = Arc::new(LocalSymbolMaterial {
            index: Arc::clone(&self.material.index),
            selected: Arc::clone(&self.material.selected),
            metadata: Arc::clone(&self.material.metadata),
            history: Arc::clone(&self.material.history),
            traversal: self.material.traversal.clone(),
            query_result: self.material.query_result.clone(),
            package_facts: self.material.package_facts.clone(),
            provider_selection: self.material.provider_selection.clone(),
            query_references: references,
        });
        self
    }
}
