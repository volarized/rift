use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::{Arc, Weak};

use rift_error::{RiftError, errors};
use rift_provider::NormalizedGraph;
use rift_syntax::DocumentPlacement;

use crate::analyzer::namespace::{SelectedFile, placed_aliases, prepare_context};
use crate::static_resolution::StaticSource;
use crate::{IndexedFile, NamespaceInput};

use super::{BuiltSemantics, PlacedFacts, PreparedSemantics, WorkspaceSemantics};

/// Input witness and provider ordinals for one completely admitted file.
#[derive(Debug)]
struct AdmittedSource {
    facts: Weak<rift_syntax::SyntaxFacts>,
    placement: Arc<DocumentPlacement>,
    rows: Range<usize>,
}

#[derive(Debug, Default)]
pub(super) struct AdmittedSources(BTreeMap<rift_core::ProjectPath, AdmittedSource>);

impl AdmittedSources {
    pub(super) fn retained_size<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        rift_protocol::retained_map_material(&self.0, tracker, |source, tracker| {
            let (placement, mut tracker) = rift_protocol::retained_arc_material(
                &source.placement,
                tracker,
                DocumentPlacement::retained_heap,
            );
            let released = if source.facts.strong_count() == 0
                && tracker.track(source.facts.as_ptr().cast::<()>())
            {
                std::mem::size_of::<rift_syntax::SyntaxFacts>() + 2 * std::mem::size_of::<usize>()
            } else {
                0
            };
            (placement.saturating_add(released), tracker)
        })
    }
}

type HeldAdmission<'a> = (
    &'a BTreeMap<rift_core::ProjectPath, AdmittedSource>,
    &'a rift_provider::ProviderPublication,
    (rift_core::SourceRevision, rift_core::TreeRevision),
);

fn reuse_captured_source(
    (sources, publication, revision): HeldAdmission<'_>,
    document: &IndexedFile,
    placed: &PlacedFacts<'_>,
    builder: &mut rift_syntax::SyntaxPublicationBuilder,
) -> Option<Arc<DocumentPlacement>> {
    let source = sources.get(placed.path)?;
    if !std::ptr::eq(source.facts.as_ptr(), document.syntax_facts().as_ref())
        || *source.placement != placed.placement
        || !builder.add_unchanged_source(
            publication,
            source.rows.clone(),
            placed.placement.unit(),
            revision,
        )
    {
        return None;
    }
    Some(Arc::clone(&source.placement))
}

/// Source and namespace evidence waiting beside an admitted project publication.
#[derive(Debug)]
pub struct PreparedProjectSemantics<'source> {
    prepared: PreparedSemantics,
    input: NamespaceInput<'source>,
    documents: Vec<&'source IndexedFile>,
    placements: Vec<PlacedFacts<'source>>,
    admitted: Vec<(Range<usize>, Option<Arc<DocumentPlacement>>)>,
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
        let (group_path, carried_records) = built.semantics.graph().group_work();
        let (carried_files, carried_rows) = self
            .admitted
            .iter()
            .filter(|(_, placement)| placement.is_some())
            .fold((0usize, 0usize), |(files, rows), (range, _)| {
                (files + 1, rows + range.len())
            });
        rift_tracing::info!(
            component = "analysis",
            operation = "analysis.captured_admission",
            files = self.admitted.len(),
            carried_files,
            carried_rows,
            group_path,
            carried_records,
            records = built.semantics.graph().records().len(),
            "Admitted captured declarations"
        );
        if resolve_static && admitted_all {
            for ((document, placed), (rows, held)) in self
                .documents
                .iter()
                .zip(self.placements)
                .zip(self.admitted)
            {
                built.semantics.captured_sources.0.insert(
                    document.path().clone(),
                    AdmittedSource {
                        facts: Arc::downgrade(document.syntax_facts()),
                        placement: held.unwrap_or_else(|| Arc::new(placed.placement)),
                        rows,
                    },
                );
            }
        }
        Ok(built)
    }

    /// Whether every supplied document survived publication admission.
    #[must_use]
    pub fn admitted_all(&self) -> bool {
        self.admitted.len() == self.documents.len()
            && self.prepared.beyond_declaration_bound.is_empty()
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
        Self::prepare_captured_project_facts_after(
            input,
            documents,
            declarations_max,
            revision,
            None,
        )
    }

    /// Admits current project declarations using unchanged syntax and placement from a
    /// previous captured workspace when available.
    ///
    /// # Errors
    /// Returns the same context, placement and publication errors as fresh admission.
    pub fn prepare_captured_project_facts_after<'source>(
        input: NamespaceInput<'source>,
        documents: &[&'source IndexedFile],
        declarations_max: usize,
        revision: u64,
        previous: Option<&Self>,
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
        let held = previous.and_then(|prior| {
            let publication = prior
                .graph
                .publications()
                .provider(&prior.syntax_provider)?;
            Some((
                &prior.captured_sources.0,
                publication,
                (prior.graph.source_revision(), prior.graph.tree_revision()),
            ))
        });
        let mut admitted = Vec::with_capacity(documents.len());
        let prepared = Self::prepare_facts_with(
            &placed,
            declarations_max,
            revision,
            true,
            |index, placed, builder| {
                let start = builder.admitted_len();
                let reused = held.and_then(|prior| {
                    reuse_captured_source(prior, documents[index], placed, builder)
                });
                if reused.is_none() {
                    builder.add_facts_placed(placed.facts, &placed.placement)?;
                }
                admitted.push((start..builder.admitted_len(), reused));
                Ok(())
            },
        )?;
        Ok(PreparedProjectSemantics {
            prepared,
            input,
            documents: documents.to_vec(),
            placements: placed,
            admitted,
            outside_namespaces,
        })
    }
}

#[cfg(test)]
mod tests;
