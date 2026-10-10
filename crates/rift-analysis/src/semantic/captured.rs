use std::collections::{BTreeMap, BTreeSet};

use rift_error::{RiftError, errors};
use rift_protocol::identity::SymbolOwner;
use rift_provider::NormalizedGraph;
use rift_syntax::DocumentPlacement;

use crate::analyzer::namespace::{SelectedFile, placed_aliases, prepare_context};
use crate::{IndexedFile, NamespaceInput};

use super::{BuiltSemantics, PlacedFacts, WorkspaceSemantics};

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
        if !matches!(
            input.owner(),
            SymbolOwner::Local | SymbolOwner::NamedLocal { .. }
        ) {
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
        Self::build_facts_placed_inner(
            &placed,
            declarations_max,
            relationships_max,
            revision,
            previous,
            true,
        )
    }
}

#[cfg(test)]
mod tests;
