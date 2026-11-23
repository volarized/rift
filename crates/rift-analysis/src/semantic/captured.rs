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
    outside_namespaces: BTreeSet<rift_core::SourceUnitId>,
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
        edges_max: usize,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        self.build_with(edges_max, previous, true)
    }

    /// Builds semantic adjacency for a source inventory that is still growing, leaving
    /// the static pass to the publication that holds every source.
    ///
    /// Static calls, references, and containment resolve across files, so a pass over a
    /// part of the sources answers for that part alone and is run again from nothing when
    /// the next sources arrive. The result holds no static occurrence and reports
    /// unavailable static call coverage for every unit.
    ///
    /// # Errors
    ///
    /// Returns a typed error when graph validation fails.
    pub fn build_deferring_static(
        self,
        edges_max: usize,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        self.build_with(edges_max, previous, false)
    }

    fn build_with(
        self,
        edges_max: usize,
        previous: Option<&NormalizedGraph>,
        resolve_static: bool,
    ) -> Result<BuiltSemantics, RiftError> {
        let admitted_all = self.admitted_all();
        let sources = if admitted_all {
            self.documents
                .iter()
                .zip(&self.placements)
                .map(|(document, placed)| StaticSource {
                    path: document.path(),
                    source: document.source(),
                    facts: document.syntax(),
                    placement: &placed.placement,
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let static_inputs = resolve_static.then_some((sources.as_slice(), &self.input));
        let mut built =
            self.prepared
                .build_with_external(edges_max, previous, None, static_inputs)?;
        built.semantics.outside_namespaces = self.outside_namespaces;
        if !resolve_static && admitted_all {
            built.semantics.defer_static_resolution(&sources);
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
        edges_max: usize,
        revision: u64,
        previous: Option<&NormalizedGraph>,
    ) -> Result<BuiltSemantics, RiftError> {
        Self::prepare_captured_project_facts(input, documents, declarations_max, revision)?
            .build(edges_max, previous)
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
                || captured
                    .get(document.path())
                    .is_none_or(|source| source.len() != document.source().len())
            {
                return errors::analysis::package_input_identity_invalid().fail();
            }
            selected.push(SelectedFile {
                path: document.path(),
                source: document.source(),
                syntax: document.syntax(),
            });
        }
        let observation_started = rift_core::bun_eligibility::start();
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
        let mut aliases = placed_aliases(
            &input,
            &mut namespace,
            &selected,
            &placements,
            declarations_max,
            || errors::analysis::package_input_identity_invalid().error(),
        )?;
        if observation_started.is_some() {
            let frameworks = selected
                .iter()
                .filter(|file| file.syntax.origin() == rift_syntax::SyntaxOrigin::Framework);
            let framework_count = frameworks.clone().count();
            let framework_first = frameworks.map(|file| file.path.as_str()).next();
            let outside_count = namespace.outside_namespaces.len();
            let alias_count = aliases.values().map(Vec::len).sum::<usize>();
            let unresolved_exports = usize::from(namespace.unresolved_exports);
            rift_core::bun_eligibility::note(
                "framework_context",
                &[
                    ("framework_sources", framework_count),
                    ("outside_namespaces", outside_count),
                    ("source_documents", selected.len()),
                ],
                selected.len(),
                framework_count + outside_count,
                framework_first
                    .map(|path| {
                        (
                            "framework context lacks a retained ownership witness",
                            Some(path),
                            None,
                        )
                    })
                    .or_else(|| {
                        namespace.outside_namespaces.iter().next().map(|path| {
                            (
                                "source outside established namespace",
                                Some(path.as_str()),
                                None,
                            )
                        })
                    }),
                observation_started,
            );
            rift_core::bun_eligibility::note(
                "alias_selector",
                &[
                    ("placed_aliases", alias_count),
                    ("unresolved_exports", unresolved_exports),
                ],
                0,
                unresolved_exports,
                namespace.unresolved_exports.then_some((
                    "namespace reports unresolved exports",
                    None,
                    None,
                )),
                None,
            );
        }
        let mut outside_namespaces = BTreeSet::new();
        for document in &mut placed {
            let path = document.path.as_str();
            if namespace.outside_namespaces.contains(path) {
                outside_namespaces.insert(document.placement.unit().clone());
            }
            document.placement = document
                .placement
                .clone()
                .with_identity_anchors(namespace.anchors.remove(path).unwrap_or_default())
                .with_logical_declarations(namespace.mappings.remove(path).unwrap_or_default())
                .with_aliases(aliases.remove(path).unwrap_or_default());
        }
        let prepared = Self::prepare_facts_placed(&placed, declarations_max, revision, true)?;
        if rift_core::bun_eligibility::active() {
            let denied = prepared.beyond_declaration_bound.len();
            let refused = prepared.refused_contributions.len();
            let first = prepared
                .beyond_declaration_bound
                .first()
                .map(|path| ("declaration cap prefixes files", Some(path.as_str()), None))
                .or_else(|| {
                    prepared.refused_contributions.first().map(|(path, _)| {
                        (
                            "declaration contribution refused",
                            Some(path.as_str()),
                            None,
                        )
                    })
                });
            rift_core::bun_eligibility::note(
                "admission_prefix",
                &[
                    ("declarations_max", declarations_max),
                    ("declaration_owners_truncated", denied),
                    ("declaration_owners_refused", refused),
                ],
                placed.len(),
                denied + refused,
                first,
                None,
            );
        }
        Ok(PreparedProjectSemantics {
            prepared,
            input,
            documents: documents.to_vec(),
            placements: placed,
            outside_namespaces,
        })
    }
}

#[cfg(test)]
mod tests;
