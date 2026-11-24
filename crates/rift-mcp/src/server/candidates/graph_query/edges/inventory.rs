//! Bounded inventory acquisition retained with its published source.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use futures_util::{StreamExt, stream};
use rift_error::RiftError;
use rift_protocol::edge_read::QueryOccurrence;
use rift_protocol::identity::SymbolIdentity;
use rift_protocol::read::{EdgeKind, ReadWarning, SymbolId, TraversalDirection};
use rift_search::sparql::{GraphLimits, SymbolRelation};

use super::{
    AcquiredAdjacency, RequestDeadline, ResolvedWorkspace, RiftMcp, coverage_failure, relation,
};

mod cache;
pub(crate) use cache::EngineInventoryCache;

/// Why the captured facts answer alone when the engine answers part of the inventory.
const ENGINE_ANSWER_INCOMPLETE: &str = "the language engine could not answer for the whole \
     inventory, so it answers from the captured static facts alone";

/// The declarations a whole-inventory read walks and the facts it reads for each.
#[derive(Clone, Copy)]
struct InventorySelection<'seeds> {
    seeds: &'seeds [SymbolId],
    facts: rift_server::EngineInventorySelection,
}

/// Calls and references one whole-inventory read collected, and whether part of the
/// inventory is missing from them.
#[derive(Debug, Default)]
pub(super) struct InventoryRead {
    pub(super) local: Vec<QueryOccurrence>,
    pub(super) outgoing_references: BTreeMap<SymbolId, Vec<usize>>,
    pub(super) calls: BTreeMap<SymbolId, rift_server::GraphCalls>,
    packages: Vec<rift_server::PackageEdge>,
    pub(super) installed: BTreeMap<SymbolId, Arc<super::InstalledTarget>>,
    pub(super) named_package_symbols: BTreeMap<
        SymbolId,
        (
            rift_protocol::read::Symbol,
            rift_search::sparql::RevisionIri,
        ),
    >,
    pub(super) local_edges: Vec<(SymbolId, SymbolRelation, SymbolId)>,
    pub(super) edge_facts: Vec<rift_protocol::index::StoredEdge>,
    outgoing_edges: BTreeMap<SymbolId, Vec<usize>>,
    incoming_edges: BTreeMap<SymbolId, Vec<usize>>,
    pub(super) warnings: Vec<ReadWarning>,
    pub(super) partial: bool,
    pub(super) records: usize,
    /// Every engine observation was a readiness refusal and the capture supplied all facts.
    pub(super) captured_readiness_only: bool,
    missing: MissingKinds,
    reference_pairs: BTreeSet<(SymbolId, SymbolId)>,
}

/// The kinds some selected declaration's captured or engine answer left incomplete.
#[derive(Debug, Default)]
struct MissingKinds {
    calls: BTreeSet<SymbolId>,
    references: BTreeSet<SymbolId>,
}

impl InventoryRead {
    /// Marks the read partial and names the kinds whose answer is incomplete for some
    /// selected declaration.
    fn report_missing(&mut self) {
        let kinds: Vec<EdgeKind> = [
            (!self.missing.calls.is_empty(), EdgeKind::Calls),
            (!self.missing.references.is_empty(), EdgeKind::References),
        ]
        .into_iter()
        .filter_map(|(missing, kind)| missing.then_some(kind))
        .collect();
        if kinds.is_empty() {
            return;
        }
        self.partial = true;
        let named: Vec<&str> = kinds.iter().map(AsRef::as_ref).collect();
        self.extend_warnings([ReadWarning::EdgeCoverageMissing {
            detail: format!(
                "the captured facts do not cover every declaration the inventory selects \
                 for {}, so edges outside that coverage are missing from this answer",
                named.join(", ")
            ),
            kinds,
        }]);
    }

    fn charge(&mut self, records: usize) -> Result<(), RiftError> {
        let maximum = GraphLimits::default().triples_max();
        self.records = self
            .records
            .checked_add(records)
            .filter(|records| *records <= maximum)
            .ok_or_else(coverage_failure)?;
        Ok(())
    }

    fn extend_warnings(&mut self, warnings: impl IntoIterator<Item = ReadWarning>) {
        for warning in warnings {
            if !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
    }

    fn admit_edge(&mut self, from: SymbolId, kind: SymbolRelation, to: SymbolId) {
        let position = self.local_edges.len();
        self.outgoing_edges
            .entry(from.clone())
            .or_default()
            .push(position);
        self.incoming_edges
            .entry(to.clone())
            .or_default()
            .push(position);
        self.local_edges.push((from, kind, to));
    }

    pub(super) fn edges(
        &self,
        seed: &SymbolId,
        kind: SymbolRelation,
        direction: TraversalDirection,
    ) -> impl Iterator<Item = &(SymbolId, SymbolRelation, SymbolId)> {
        let positions = match direction {
            TraversalDirection::Outgoing => self.outgoing_edges.get(seed),
            TraversalDirection::Incoming => self.incoming_edges.get(seed),
        };
        positions
            .into_iter()
            .flatten()
            .filter_map(|position| self.local_edges.get(*position))
            .filter(move |(_, relation, _)| *relation == kind)
    }

    fn admit_references(
        &mut self,
        seed: &SymbolId,
        occurrences: Vec<QueryOccurrence>,
    ) -> Result<(), RiftError> {
        self.admit_reference_read(seed, occurrences, false)
    }

    fn admit_occurrences(
        &mut self,
        seed: &SymbolId,
        occurrences: Vec<QueryOccurrence>,
    ) -> Result<(), RiftError> {
        self.admit_reference_read(seed, occurrences, true)
    }

    fn admit_reference_read(
        &mut self,
        seed: &SymbolId,
        occurrences: Vec<QueryOccurrence>,
        physical: bool,
    ) -> Result<(), RiftError> {
        for occurrence in occurrences {
            let pair = occurrence
                .enclosing
                .as_ref()
                .map(|caller| (caller.clone(), seed.clone()));
            if !physical
                && pair
                    .as_ref()
                    .is_none_or(|pair| self.reference_pairs.contains(pair))
            {
                continue;
            }
            self.charge(1)?;
            if let Some((caller, target)) = pair {
                if self
                    .reference_pairs
                    .insert((caller.clone(), target.clone()))
                {
                    self.admit_edge(caller.clone(), SymbolRelation::References, target);
                }
                self.outgoing_references
                    .entry(caller)
                    .or_default()
                    .push(self.local.len());
            }
            self.local.push(occurrence);
        }
        Ok(())
    }

    fn admit_calls(
        &mut self,
        seed: &SymbolId,
        calls: rift_server::GraphCalls,
        replace: bool,
    ) -> Result<(), RiftError> {
        self.charge(calls.record_count())?;
        if replace || !self.calls.contains_key(seed) {
            self.calls.insert(seed.clone(), calls);
        } else if let Some(held) = self.calls.get_mut(seed) {
            held.merge(calls);
        }
        Ok(())
    }
}

/// Reads the inventory from the captured static facts alone.
fn captured_inventory(
    reads: &rift_server::ReadService,
    inventory: &InventorySelection<'_>,
) -> Result<InventoryRead, RiftError> {
    let mut read = InventoryRead::default();
    for seed in inventory.seeds {
        if inventory.facts.references {
            let references =
                rift_server::captured_graph_references(reads, seed, inventory.facts.enclosing)?;
            read.charge(references.edges.len())?;
            read.edge_facts.extend(references.edges);
            if !references.complete {
                read.missing.references.insert(seed.clone());
            }
            if inventory.facts.occurrences.is_some() {
                read.admit_occurrences(seed, references.occurrences)?;
            } else {
                read.admit_references(seed, references.occurrences)?;
            }
        }
        if inventory.facts.calls {
            let calls =
                rift_server::captured_graph_calls(reads, seed, TraversalDirection::Outgoing)?;
            if !calls.complete {
                read.missing.calls.insert(seed.clone());
            }
            read.admit_calls(seed, calls, true)?;
        }
    }
    read.report_missing();
    Ok(read)
}

impl RiftMcp {
    /// Acquires the calls, references and usages a whole-inventory query reads.
    ///
    /// The captured static facts answer first. Configured engines read every established
    /// declaration in file groups, bounded by the source configuration, worker count,
    /// pending requests, dataset capacity, and request deadline. A failed group keeps the
    /// captured facts and earlier engine answers, and reports the missing analysis.
    pub(in crate::server::candidates::graph_query) async fn acquire_semantic_inventory(
        &self,
        resolved: &ResolvedWorkspace,
        required: rift_search::sparql::SemanticInventory,
        deadline: RequestDeadline,
    ) -> Result<AcquiredAdjacency, RiftError> {
        let reads = &resolved.published.reads;
        let mut acquired = AcquiredAdjacency::default();
        acquired.capture_edge_inventory(reads, required)?;
        let engines = self.engine_pool_for(&resolved.published).await;
        let full_adjacency = required.complete_adjacency
            && (required.adjacency.calls
                || required.adjacency.references
                || required.occurrences.is_some());
        let resolved_adjacency = required.adjacency.calls
            || required.adjacency.references
            || required.adjacency.type_uses
            || required.adjacency.heritage.extends
            || required.adjacency.heritage.implements
            || required.occurrences.is_some();
        if (!reads.uses_edge_engines(&engines) && !full_adjacency) || !resolved_adjacency {
            return Ok(acquired);
        }
        let references = required.adjacency.references || required.occurrences.is_some();
        let enclosing = required.adjacency.references
            || required.occurrences.is_some_and(|usage| usage.enclosing);
        let capture = Arc::clone(reads);
        let (seeds, coverage) = self
            .blocking
            .run("semantic inventory seeds", move || {
                capture.local_search_view_input(true)?.inventory_seeds()
            })
            .await?;
        acquired.partial |= coverage == rift_server::InventoryCoverage::Partial;
        let mut served = BTreeMap::new();
        for seed in &seeds {
            let identity = SymbolIdentity::parse(seed.as_str())?;
            let language = identity.language();
            if engines.engine_for(language).is_some() {
                served
                    .entry(language.identity_segment())
                    .or_insert_with(|| language.clone());
            }
        }
        let workers = resolved
            .published
            .configuration
            .server_configuration()
            .num_workers;
        let inventory = InventorySelection {
            seeds: &seeds,
            facts: rift_server::EngineInventorySelection {
                calls: required.adjacency.calls,
                references,
                enclosing,
                occurrences: required.occurrences,
                types: rift_search::sparql::TypeSelection {
                    definitions: required.adjacency.type_uses,
                    extends: required.adjacency.heritage.extends,
                    implements: required.adjacency.heritage.implements,
                },
                batch_inputs: usize::try_from(workers).unwrap_or(1),
                records_max: GraphLimits::default().triples_max(),
            },
        };
        let engine = if served.is_empty() {
            Err(ENGINE_ANSWER_INCOMPLETE)
        } else if resolved.stale.is_some() {
            Err(
                "the published index lags the captured tree, so the inventory answers from \
                 the captured static facts alone",
            )
        } else {
            self.inventory_within_budget(resolved, &engines, &inventory, deadline)
                .await?
        };
        let read = match engine {
            Ok(engine) => engine,
            Err(detail) => {
                self.captured_fallback(reads, &inventory, served, detail)
                    .await?
            }
        };
        acquired.admit_inventory(&read)?;
        // Physical usages come from the reference inventory. Stream captured facts as
        // well only when the query also selects their relations or original records.
        acquired.captured_inventory = (required.edges.relations
            || required.edges.metadata
            || required.adjacency.calls
            || required.adjacency.references)
            .then_some(required);
        Ok(acquired)
    }

    async fn captured_fallback(
        &self,
        reads: &Arc<rift_server::ReadService>,
        inventory: &InventorySelection<'_>,
        served: BTreeMap<String, rift_protocol::read::Language>,
        detail: &str,
    ) -> Result<Arc<InventoryRead>, RiftError> {
        let capture = Arc::clone(reads);
        let seeds = inventory.seeds.to_vec();
        let selection = inventory.facts;
        let mut captured = self
            .blocking
            .run("captured edge inventory", move || {
                captured_inventory(
                    &capture,
                    &InventorySelection {
                        seeds: &seeds,
                        facts: selection,
                    },
                )
            })
            .await?;
        for language in served.into_values() {
            captured.partial = true;
            captured
                .warnings
                .push(ReadWarning::EngineAnalysisUnavailable {
                    language: Some(language),
                    detail: detail.to_owned(),
                });
        }
        Ok(Arc::new(captured))
    }

    /// The engine's inventory, or the detail of why the captured facts answer alone.
    ///
    /// The walk's end bounds the engine's own answers. Mapping them, naming their targets,
    /// and revalidating the tree come after it, and their cost is unknown until they run, so
    /// the whole acquisition ends while the graph can still be evaluated from the captured
    /// facts.
    ///
    /// # Cancel safety
    ///
    /// Dropping the future abandons the acquisition, as its own deadline does, and retains
    /// no inventory.
    async fn inventory_within_budget(
        &self,
        resolved: &ResolvedWorkspace,
        engines: &Arc<rift_server::EnginePool>,
        inventory: &InventorySelection<'_>,
        deadline: RequestDeadline,
    ) -> Result<Result<Arc<InventoryRead>, &'static str>, RiftError> {
        let end = deadline.graph_walk_end(resolved.capture_elapsed);
        let acquired = tokio::time::timeout_at(
            deadline.graph_acquisition_end(),
            Box::pin(self.engine_inventory(resolved, engines, inventory, end)),
        )
        .await;
        match acquired {
            Ok(Ok(engine)) => Ok(Ok(engine)),
            Ok(Err(error)) if super::super::lacks_coverage(&error) => {
                Ok(Err(ENGINE_ANSWER_INCOMPLETE))
            }
            Ok(Err(error)) => Err(error),
            Err(_elapsed) => Ok(Err(
                "`[server] readiness_timeout` was spent before the language engine's answer was \
                 checked against the captured tree, so the inventory answers from the captured \
                 static facts alone",
            )),
        }
    }

    /// Reads file groups through engine batches and maps their locations on the worker pool.
    async fn engine_inventory(
        &self,
        resolved: &ResolvedWorkspace,
        engines: &Arc<rift_server::EnginePool>,
        inventory: &InventorySelection<'_>,
        end: tokio::time::Instant,
    ) -> Result<Arc<InventoryRead>, RiftError> {
        let mut cache = resolved.published.engine_inventory.entries.lock().await;
        if let Some(read) = cache.get(inventory.facts, tokio::time::Instant::now()) {
            return Ok(read);
        }
        let read = self
            .read_engine_inventory(resolved, engines, inventory, end)
            .await?;
        // The request already accepted this publication. Every engine file refused
        // readiness, so there are no live facts or new source positions to validate.
        if read.captured_readiness_only {
            return Ok(Arc::new(read));
        }
        if !self
            .engine_tree_matches(&resolved.published)
            .await
            .map_err(|_| coverage_failure())?
        {
            return Err(coverage_failure());
        }
        let read = Arc::new(read);
        cache.remember(inventory.facts, Arc::clone(&read), end);
        Ok(read)
    }

    async fn read_engine_inventory(
        &self,
        resolved: &ResolvedWorkspace,
        engines: &Arc<rift_server::EnginePool>,
        inventory: &InventorySelection<'_>,
        end: tokio::time::Instant,
    ) -> Result<InventoryRead, RiftError> {
        let reads = Arc::clone(&resolved.published.reads);
        let seeds = inventory.seeds.to_vec();
        let capture = Arc::clone(&reads);
        let selection = inventory.facts;
        let (files, mut read, sources) = self
            .blocking
            .run("engine inventory files", move || {
                let inventory = InventorySelection {
                    seeds: &seeds,
                    facts: selection,
                };
                let sources = if selection.references {
                    rift_server::DocumentSources::of(&capture)
                } else {
                    rift_server::DocumentSources::default()
                };
                Ok((
                    capture.engine_inventory_files(&seeds, selection)?,
                    captured_inventory(&capture, &inventory)?,
                    sources,
                ))
            })
            .await?;
        let capture = Arc::clone(&reads);
        let engine_capture = Arc::clone(engines);
        let roots = self
            .blocking
            .run("engine inventory package roots", move || {
                Ok(rift_server::CalleeRoots::read(&capture, &engine_capture))
            })
            .await?;
        let workers = selection.batch_inputs.max(1);
        let requests = stream::iter(files.into_iter().map(|file| {
            let capture = Arc::clone(&reads);
            let confirmations = Arc::clone(&reads);
            let roots = roots.clone();
            let sources = sources.clone();
            async move {
                let cutoff = rift_server::EngineWalkDeadline::AdmitBefore(end);
                let answer = tokio::time::timeout_at(
                    end + crate::server::ENGINE_READ_GRACE,
                    file.read(engines, (selection, &sources), cutoff),
                )
                .await
                .map_err(|_| {
                    rift_error::errors::lsp::engine_analyzing()
                        .attempts(0_u64)
                        .error()
                })??;
                let Some(answer) = answer else {
                    return Ok(None);
                };
                let inventory = self
                    .blocking
                    .run("engine inventory locations", move || {
                        answer.resolve(&capture, &roots)
                    })
                    .await?;
                Ok(Some(
                    inventory.confirm_calls(&confirmations, engines, end).await,
                ))
            }
        }))
        .buffer_unordered(workers);
        tokio::pin!(requests);
        read.partial = false;
        read.warnings
            .retain(|warning| !matches!(warning, ReadWarning::EdgeCoverageMissing { .. }));
        let mut coverage = EngineCoverage {
            occurrences: inventory.facts.occurrences.is_some(),
            ..EngineCoverage::default()
        };
        while let Some(answer) = requests.next().await {
            coverage.admit(&mut read, answer)?;
        }
        read.captured_readiness_only = coverage.finish(&mut read);
        if !read.captured_readiness_only {
            self.name_inventory_targets(
                resolved,
                engines,
                roots.clone(),
                &mut read,
                end,
                selection.occurrences.is_some(),
            )
            .await?;
        }
        read.report_missing();
        Ok(read)
    }

    async fn name_inventory_targets(
        &self,
        resolved: &ResolvedWorkspace,
        engines: &Arc<rift_server::EnginePool>,
        roots: rift_server::CalleeRoots,
        read: &mut InventoryRead,
        end: tokio::time::Instant,
        physical: bool,
    ) -> Result<(), RiftError> {
        let mut walk = super::AdjacencyWalk::new(
            self,
            resolved,
            None,
            rift_search::sparql::AdjacencyStep {
                kind: EdgeKind::Calls,
                direction: TraversalDirection::Outgoing,
            },
            GraphLimits::default(),
            RequestDeadline {
                at: end,
                budget: end.saturating_duration_since(tokio::time::Instant::now()),
            },
        );
        walk.local = Some((Arc::clone(engines), roots));
        let mut added = 0usize;
        for (seed, calls) in &mut read.calls {
            let before = calls.record_count();
            walk.name_package_calls(calls, true).await?;
            let after = calls.record_count();
            added = added
                .checked_add(after.saturating_sub(before))
                .ok_or_else(coverage_failure)?;
            if !calls.complete {
                read.missing.calls.insert(seed.clone());
            }
        }
        read.charge(added)?;
        let packages = std::mem::take(&mut read.packages);
        let mut grouped = BTreeMap::<EdgeKind, Vec<rift_server::PackageEdge>>::new();
        for package in packages {
            grouped.entry(package.kind).or_default().push(package);
        }
        for (kind, targets) in grouped {
            let callees = targets
                .iter()
                .map(|target| target.target.clone())
                .collect::<Vec<_>>();
            let named = walk
                .name_package_targets_unreported(&callees, kind, true)
                .await?;
            let mut dropped = 0usize;
            for (target, identity) in targets.into_iter().zip(named) {
                // An engine target the static pass named at the same site keeps that name.
                let Some(identity) = identity.or_else(|| target.static_target.clone()) else {
                    dropped = dropped.saturating_add(1);
                    continue;
                };
                let edge = target.named(identity);
                if kind == EdgeKind::References
                    && let Some(source) = edge.occurrence.clone()
                {
                    let occurrence = QueryOccurrence {
                        target: edge.to.clone(),
                        enclosing: Some(edge.from.clone()),
                        source,
                        kind,
                    };
                    if physical {
                        read.admit_occurrences(&edge.to, vec![occurrence])?;
                    } else {
                        read.admit_references(&edge.to, vec![occurrence])?;
                    }
                } else {
                    read.admit_edge(edge.from.clone(), relation(kind)?, edge.to.clone());
                }
                read.edge_facts.push(edge);
            }
            walk.report_missing_targets(kind, dropped);
        }
        walk.verify_installed().await?;
        read.partial |= walk.acquired.partial;
        read.extend_warnings(walk.acquired.warnings);
        read.installed = walk.acquired.installed;
        read.named_package_symbols = walk.acquired.named_package_symbols;
        Ok(())
    }

    pub(super) async fn inventory_for_kind(
        &self,
        resolved: &ResolvedWorkspace,
        kind: EdgeKind,
        seed: Option<&SymbolId>,
        occurrences: Option<rift_search::sparql::OccurrenceSelection>,
        end: tokio::time::Instant,
    ) -> Result<Arc<InventoryRead>, RiftError> {
        let workers = resolved
            .published
            .configuration
            .server_configuration()
            .num_workers;
        let facts = rift_server::EngineInventorySelection {
            calls: kind == EdgeKind::Calls,
            references: kind == EdgeKind::References,
            enclosing: kind == EdgeKind::References,
            occurrences,
            types: rift_search::sparql::TypeSelection {
                definitions: kind == EdgeKind::HasType,
                extends: kind == EdgeKind::Extends,
                implements: kind == EdgeKind::Implements,
            },
            batch_inputs: usize::try_from(workers).unwrap_or(1),
            records_max: GraphLimits::default().triples_max(),
        };
        {
            let mut cache = resolved.published.engine_inventory.entries.lock().await;
            if let Some(read) = cache.get(facts, tokio::time::Instant::now()) {
                return Ok(read);
            }
        }
        if let Some(seed) = seed {
            let engines = self.engine_pool_for(&resolved.published).await;
            let seeds = [seed.clone()];
            return self
                .read_engine_inventory(
                    resolved,
                    &engines,
                    &InventorySelection {
                        seeds: &seeds,
                        facts,
                    },
                    end,
                )
                .await
                .map(Arc::new);
        }
        let capture = Arc::clone(&resolved.published.reads);
        let seeds = self
            .blocking
            .run("engine inventory seeds", move || {
                Ok(capture.local_search_view_input(true)?.inventory_seeds()?.0)
            })
            .await?;
        let engines = self.engine_pool_for(&resolved.published).await;
        self.engine_inventory(
            resolved,
            &engines,
            &InventorySelection {
                seeds: &seeds,
                facts,
            },
            end,
        )
        .await
    }
}

#[derive(Default)]
struct EngineCoverage {
    occurrences: bool,
    file_observations: FileObservations,
    seen_calls: BTreeSet<SymbolId>,
    calls: BTreeMap<SymbolId, bool>,
    references: BTreeMap<SymbolId, bool>,
}

#[derive(Default, PartialEq, Eq)]
enum FileObservations {
    #[default]
    None,
    OnlyReadinessRefusals,
    Other,
}

impl EngineCoverage {
    fn admit(
        &mut self,
        read: &mut InventoryRead,
        answer: Result<Option<rift_server::EngineInventory>, RiftError>,
    ) -> Result<(), RiftError> {
        match (&self.file_observations, &answer) {
            // A file with no engine adds no observation. A ready engine returns a
            // report even when it found no facts.
            (_, Ok(None)) => {}
            (FileObservations::None | FileObservations::OnlyReadinessRefusals, Err(error))
                if error.slug() == rift_error::errors::lsp::engine_analyzing::SLUG =>
            {
                self.file_observations = FileObservations::OnlyReadinessRefusals;
            }
            _ => self.file_observations = FileObservations::Other,
        }
        match answer {
            Ok(Some(answer)) => {
                read.partial |= answer.warnings.iter().any(|warning| {
                    matches!(
                        warning,
                        ReadWarning::EdgeCoverageMissing { .. }
                            | ReadWarning::EngineAnalysisUnavailable { .. }
                    )
                });
                read.charge(answer.packages.len())?;
                read.packages.extend(answer.packages);
                read.extend_warnings(answer.warnings);
                for (seed, calls) in answer.calls {
                    self.calls
                        .entry(seed.clone())
                        .and_modify(|complete| *complete &= calls.complete)
                        .or_insert(calls.complete);
                    read.extend_warnings(calls.edge_warnings.iter().cloned());
                    let replace = self.seen_calls.insert(seed.clone());
                    read.admit_calls(&seed, calls, replace)?;
                }
                for (seed, references) in answer.references {
                    self.references
                        .entry(seed.clone())
                        .and_modify(|complete| *complete &= references.complete)
                        .or_insert(references.complete);
                    read.charge(references.edges.len())?;
                    read.edge_facts.extend(references.edges);
                    read.extend_warnings(references.edge_warnings);
                    if self.occurrences {
                        read.admit_occurrences(&seed, references.occurrences)?;
                    } else {
                        read.admit_references(&seed, references.occurrences)?;
                    }
                }
                for edge in answer.types.into_iter().chain(answer.heritage) {
                    read.charge(1)?;
                    let kind = edge.kinds.first().copied().ok_or_else(coverage_failure)?;
                    read.admit_edge(edge.from.clone(), relation(kind)?, edge.to.clone());
                    read.edge_facts.push(edge);
                }
            }
            Ok(None) => {}
            Err(error) => {
                read.partial = true;
                read.extend_warnings([ReadWarning::EngineAnalysisUnavailable {
                    language: None,
                    detail: if error.slug() == rift_error::errors::lsp::engine_analyzing::SLUG {
                        format!("the language engine was still analyzing when `[server] readiness_timeout` was spent, so this inventory carries none of its edges from that file: {}", error.detail())
                    } else { error.detail() },
                }]);
            }
        }
        Ok(())
    }

    /// Completes coverage and returns whether only the accepted capture supplies facts.
    fn finish(self, read: &mut InventoryRead) -> bool {
        for (seed, complete) in self.calls {
            if complete {
                read.missing.calls.remove(&seed);
            }
        }
        for (seed, complete) in self.references {
            if complete {
                read.missing.references.remove(&seed);
            }
        }
        self.file_observations == FileObservations::OnlyReadinessRefusals
            && read.packages.is_empty()
            && read.calls.values().all(|calls| calls.packages.is_empty())
    }
}

#[cfg(test)]
mod tests;
