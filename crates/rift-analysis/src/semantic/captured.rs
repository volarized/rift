use std::collections::{BTreeMap, BTreeSet};

use rift_error::{RiftError, errors};
use rift_provider::NormalizedGraph;
use rift_syntax::DocumentPlacement;

use crate::analyzer::namespace::{SelectedFile, placed_aliases, prepare_context};
use crate::static_resolution::StaticSource;
use crate::{IndexedFile, NamespaceInput};

use super::{BuiltSemantics, PlacedFacts, PreparedSemantics, WorkspaceSemantics};

/// Source and namespace evidence waiting beside an admitted project publication.
#[derive(Debug)]
pub struct PreparedProjectSemantics<'source> {
    prepared: PreparedSemantics,
    input: NamespaceInput<'source>,
    documents: Vec<&'source IndexedFile>,
    placements: Vec<PlacedFacts<'source>>,
}

impl PreparedProjectSemantics<'_> {
    /// Documents outside the accepted declaration capacity.
    #[must_use]
    pub fn beyond_declaration_bound(&self) -> &[rift_core::ProjectPath] {
        &self.prepared.beyond_declaration_bound
    }

    /// Removes contribution refusals before the caller retries admission.
    pub fn take_refused_contributions(&mut self) -> Vec<(rift_core::ProjectPath, RiftError)> {
        std::mem::take(&mut self.prepared.refused_contributions)
    }

    /// Builds semantic adjacency for the admitted source inventory.
    ///
    /// # Errors
    ///
    /// Returns a typed error when graph validation fails.
    pub fn build(
        self,
        relationships_max: usize,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        let mut built = self.prepared.build(relationships_max, previous)?;
        if built.beyond_declaration_bound.is_empty() && built.refused_contributions.is_empty() {
            let sources = self
                .documents
                .iter()
                .zip(&self.placements)
                .map(|(document, placed)| StaticSource {
                    path: document.path(),
                    source: document.source(),
                    facts: document.syntax(),
                    placement: &placed.placement,
                })
                .collect::<Vec<_>>();
            built
                .semantics
                .add_static_resolution(&sources, &self.input, relationships_max);
        }
        Ok(built)
    }

    /// Whether every supplied document survived publication admission.
    #[must_use]
    pub fn admitted_all(&self) -> bool {
        self.prepared.beyond_declaration_bound.is_empty()
            && self.prepared.refused_contributions.is_empty()
    }
}

impl WorkspaceSemantics {
    /// Builds project semantics from one captured owner, source set, and metadata set.
    ///
    /// Namespace placement uses the supplied retained facts and captured source bytes.
    /// Physical units remain project units for both local and named local owners.
    ///
    /// # Errors
    /// Returns the existing input refusal for an owner or source outside the captured context.
    pub fn build_captured_project_facts(
        input: NamespaceInput<'_>,
        documents: &[&IndexedFile],
        declarations_max: usize,
        relationships_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        Self::prepare_captured_project_facts(input, documents, declarations_max, revision)?
            .build(relationships_max, previous)
    }

    /// Admits captured project declarations, including namespace aliases, before graph construction.
    ///
    /// # Errors
    /// Returns the existing input refusal for an owner or source outside the captured context.
    pub fn prepare_captured_project_facts<'source>(
        input: NamespaceInput<'source>,
        documents: &[&'source IndexedFile],
        declarations_max: usize,
        revision: u64,
    ) -> Result<PreparedProjectSemantics<'source>, RiftError> {
        if !input.owner().is_local() {
            return errors::analysis::package_input_identity_invalid().fail();
        }
        let captured = input
            .files()
            .iter()
            .map(|source| (source.path(), source.text()))
            .collect::<BTreeMap<_, _>>();
        if documents.len() > captured.len() {
            return errors::analysis::package_input_identity_invalid().fail();
        }
        let mut selected = Vec::with_capacity(documents.len());
        let mut paths = BTreeSet::new();
        for document in documents {
            if !paths.insert(document.path())
                || captured.get(document.path()).copied() != Some(document.source())
            {
                return errors::analysis::package_input_identity_invalid().fail();
            }
            selected.push(SelectedFile {
                path: document.path(),
                source: document.source(),
                syntax: document.syntax(),
            });
        }
        let mut namespace = prepare_context(&input, &selected);
        let mut placed = documents
            .iter()
            .map(|document| {
                Ok(PlacedFacts {
                    facts: document.syntax(),
                    path: document.path(),
                    placement: DocumentPlacement::project_path(document.path())?,
                })
            })
            .collect::<Result<Vec<_>, RiftError>>()?;
        let placements = placed
            .iter()
            .map(|document| &document.placement)
            .collect::<Vec<_>>();
        let mut aliases = placed_aliases(&namespace, &selected, &placements, || {
            errors::analysis::package_input_identity_invalid().error()
        })?;
        for document in &mut placed {
            let path = document.path.as_str();
            document.placement = document
                .placement
                .clone()
                .with_identity_anchors(namespace.anchors.remove(path).unwrap_or_default())
                .with_logical_declarations(namespace.mappings.remove(path).unwrap_or_default())
                .with_aliases(aliases.remove(path).unwrap_or_default());
        }
        let prepared = Self::prepare_facts_placed(&placed, declarations_max, revision, true)?;
        Ok(PreparedProjectSemantics {
            prepared,
            input,
            documents: documents.to_vec(),
            placements: placed,
        })
    }
}

#[cfg(test)]
mod tests;
