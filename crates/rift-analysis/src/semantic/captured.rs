use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;

use get_size2::GetSize;
use rift_core::ProjectPath;

use rift_error::{RiftError, errors};
use rift_provider::{ModifiedContributionRun, NormalizedGraph};
use rift_syntax::{DocumentPlacement, SyntaxFacts};

use crate::analyzer::namespace::{SelectedFile, placed_aliases, prepare_context};
use crate::static_resolution::StaticSource;
use crate::{IndexedFile, NamespaceInput};

use super::{BuiltSemantics, PlacedFacts, PreparedSemantics, WorkspaceSemantics};

#[derive(Debug)]
pub(super) struct CapturedRuns {
    scope: String,
    declarations_max: usize,
    rows: Box<[DocumentRun]>,
}

#[derive(Debug)]
struct DocumentRun {
    syntax: Arc<SyntaxFacts>,
    placement: Arc<DocumentPlacement>,
    positions: Range<usize>,
}

impl GetSize for CapturedRuns {
    fn get_heap_size_with_tracker<T: get_size2::GetSizeTracker>(&self, tracker: T) -> (usize, T) {
        let (rows, tracker) = rift_protocol::retained_sequence_material(
            self.rows.iter(),
            std::mem::size_of_val(self.rows.as_ref()),
            tracker,
            |run, tracker| {
                let (placement, tracker) = rift_protocol::retained_arc_material(
                    &run.placement,
                    tracker,
                    GetSize::get_heap_size_with_tracker,
                );
                let (syntax, tracker) = rift_protocol::retained_arc_material(
                    &run.syntax,
                    tracker,
                    GetSize::get_heap_size_with_tracker,
                );
                (placement.saturating_add(syntax), tracker)
            },
        );
        (rows.saturating_add(self.scope.capacity()), tracker)
    }
}

/// Source and namespace evidence waiting beside an admitted project publication.
#[derive(Debug)]
pub struct PreparedProjectSemantics<'source> {
    prepared: PreparedSemantics,
    input: NamespaceInput<'source>,
    documents: Vec<&'source IndexedFile>,
    placements: Vec<PlacedFacts<'source>>,
    outside_namespaces: BTreeSet<rift_core::SourceUnitId>,
    positions: Vec<Range<usize>>,
    declarations_max: usize,
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
        self.build_with(edges_max, previous, true, None)
    }

    /// Builds a named rebuild after the supplied predecessor's selected paths changed.
    ///
    /// # Errors
    /// Returns the ordinary graph validation errors.
    pub fn build_after_modified_paths(
        self,
        edges_max: usize,
        previous: Option<&NormalizedGraph>,
        predecessor: &WorkspaceSemantics,
        paths: &[&ProjectPath],
    ) -> Result<BuiltSemantics, RiftError> {
        self.build_with(edges_max, previous, true, Some((predecessor, paths)))
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
        self.build_with(edges_max, previous, false, None)
    }

    fn build_with(
        self,
        edges_max: usize,
        previous: Option<&NormalizedGraph>,
        resolve_static: bool,
        edit: Option<(&WorkspaceSemantics, &[&ProjectPath])>,
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
        let scope =
            (resolve_static && admitted_all && self.positions.len() == self.documents.len())
                .then(|| self.input.owner().local_scope().ok())
                .flatten();
        let modification = scope.as_deref().and_then(|scope| {
            let (predecessor, paths) = edit?;
            let graph = previous.filter(|graph| std::ptr::eq(*graph, predecessor.graph()))?;
            let captured = predecessor.captured_runs.as_ref()?;
            let runs = self.modified_runs(scope, paths, captured)?;
            Some((graph, captured, runs))
        });
        let static_inputs = resolve_static.then_some((sources.as_slice(), &self.input));
        let mut built = if let Some(static_inputs) = static_inputs.filter(|_| scope.is_some()) {
            self.prepared.build_captured(
                edges_max,
                static_inputs,
                modification
                    .as_ref()
                    .map(|(graph, _, runs)| (*graph, runs.as_slice())),
            )?
        } else {
            self.prepared
                .build_with_external(edges_max, previous, None, static_inputs)?
        };
        built.semantics.outside_namespaces = self.outside_namespaces;
        if !resolve_static && admitted_all {
            built.semantics.defer_static_resolution(&sources);
        }
        if let Some(scope) = scope {
            built.semantics.captured_runs = Some(CapturedRuns {
                scope,
                declarations_max: self.declarations_max,
                rows: self
                    .documents
                    .iter()
                    .zip(self.placements)
                    .zip(self.positions)
                    .enumerate()
                    .map(|(position, ((document, placed), positions))| DocumentRun {
                        syntax: Arc::clone(document.syntax_facts()),
                        placement: modification
                            .as_ref()
                            .and_then(|(_, captured, _)| {
                                let old = &captured.rows[position];
                                let (_, paths) = edit?;
                                paths
                                    .binary_search(&document.path())
                                    .is_err()
                                    .then(|| Arc::clone(&old.placement))
                            })
                            .unwrap_or_else(|| Arc::new(placed.placement)),
                        positions,
                    })
                    .collect(),
            });
        }
        Ok(built)
    }

    fn modified_runs(
        &self,
        scope: &str,
        paths: &[&ProjectPath],
        captured: &CapturedRuns,
    ) -> Option<Vec<ModifiedContributionRun>> {
        const MAX_EDITS: usize = 64;
        if paths.is_empty()
            || paths.len() > MAX_EDITS
            || !paths.windows(2).all(|pair| pair[0] < pair[1])
            || scope != captured.scope
            || self.declarations_max != captured.declarations_max
            || self.documents.len() != captured.rows.len()
        {
            return None;
        }
        let mut modified = Vec::with_capacity(paths.len());
        for (((document, placed), positions), old) in self
            .documents
            .iter()
            .zip(&self.placements)
            .zip(&self.positions)
            .zip(&captured.rows)
        {
            let unit = placed.placement.unit();
            if old.placement.unit() != unit {
                return None;
            }
            if paths.binary_search(&document.path()).is_ok() {
                modified.push(ModifiedContributionRun {
                    unit: unit.clone(),
                    previous: old.positions.clone(),
                    current: positions.clone(),
                });
            } else if !Arc::ptr_eq(document.syntax_facts(), &old.syntax)
                || placed.placement != *old.placement
            {
                return None;
            }
        }
        (modified.len() == paths.len()).then_some(modified)
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
        let mut positions = Vec::with_capacity(placed.len());
        let prepared =
            Self::prepare_facts_placed(&placed, declarations_max, revision, true, |range| {
                positions.push(range)
            })?;
        Ok(PreparedProjectSemantics {
            prepared,
            input,
            documents: documents.to_vec(),
            placements: placed,
            outside_namespaces,
            positions,
            declarations_max,
        })
    }
}

#[cfg(test)]
mod tests;
