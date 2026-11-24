//! Edge traversal through configured language engines: incoming references,
//! and outgoing calls through call hierarchy.

mod inventory;
pub use inventory::{
    EngineInventory, EngineInventoryAnswer, EngineInventoryFile, EngineInventorySelection,
    PackageEdge,
};
use rift_protocol::index::StoredEdge;

use std::collections::{BTreeMap, BTreeSet};

use lsp_types::{CallHierarchyOutgoingCall, Location, Position, Range, Uri};
use rift_core::{ProjectPath, SymbolId as CoreSymbolId};
use rift_error::errors;
use rift_index::{EdgeStore, IndexedFile};
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::position::LineIndex;
use rift_lsp::session::EngineSession;
use rift_lsp::uri::TreeRoot;
use rift_protocol::edge_read::QueryOccurrence;
use rift_protocol::read::{
    EdgeDerivation, EdgeKind, EdgeProvider, ExactKind, Extensions, GraphHop, HopDirection,
    Language, ReadWarning, SearchParams, SearchParamsTarget, SearchTraversal, SourceUnitId,
    SourceUnitSpan, Symbol, SymbolId, TextRange, TraversalDirection,
};
use rift_syntax::SyntaxSymbol;
use tokio::time::Instant;

use crate::callee::{
    CalleeDeclaration, CalleeFile, CalleeRoots, NamedCallee, PackageCallee, callee_file,
};
use crate::engine::{EnginePool, EngineSlot, EngineWalkDeadline, OutgoingAnswer, SessionFuture};
use crate::read::{ReadService, RiftError, symbol_id};
use crate::traversal::{
    TRAVERSAL_SYMBOLS_MAX, engine_kind, resolve_graph_symbol, validate_traversal,
    walk_traversal_with_references,
};

/// References and calls resolved from one published source revision.
#[derive(Debug, Default)]
pub struct EngineReferences {
    revision: Option<String>,
    incoming: BTreeMap<CoreSymbolId, Vec<GraphHop>>,
    outgoing: BTreeMap<CoreSymbolId, Vec<GraphHop>>,
    /// Callees in package files, each waiting for the global API to name its declaration.
    package_callees: Vec<PackageCallee>,
    /// The package declarations the global API named, each the end of an outgoing edge.
    package_declarations: BTreeMap<CoreSymbolId, PackageDeclaration>,
    /// Edges to callees the walk could not name: under no root, answered by no
    /// declaration, or left unasked.
    dropped_callees: u64,
    /// Engines whose answer the walk took unconfirmed, quiet past `settle_delay`.
    unconfirmed: BTreeSet<String>,
    analysis_unavailable: Option<ReadWarning>,
    registry_unresolved: bool,
}

impl EngineReferences {
    /// Whether the engines contributed any traversal edges.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.incoming.values().all(Vec::is_empty) && self.outgoing.values().all(Vec::is_empty)
    }

    /// The warning this read carries when the engine tier contributed nothing it could
    /// stand behind. Absent when every consulted engine answered.
    #[must_use]
    pub fn analysis_unavailable(&self) -> Option<&ReadWarning> {
        self.analysis_unavailable.as_ref()
    }

    /// Every warning the engine tier adds to this read, in wire order: the dropped
    /// contribution, unresolved registry roots, the callees an outgoing walk dropped,
    /// then the engines it took unconfirmed. Empty when every consulted engine answered in full.
    #[must_use]
    pub fn warnings(&self) -> Vec<ReadWarning> {
        let mut warnings: Vec<ReadWarning> = self.analysis_unavailable.iter().cloned().collect();
        if self.registry_unresolved {
            warnings.push(ReadWarning::PackageContextDegraded {
                resolver: "cargo".to_owned(),
                reason: "Cargo cache source folders do not establish the registry endpoint; their callees are unavailable.".to_owned(),
            });
        }
        if self.dropped_callees > 0 {
            warnings.push(callees_dropped_warning(self.dropped_callees));
        }
        if !self.unconfirmed.is_empty() {
            warnings.push(readiness_unconfirmed_warning(&self.unconfirmed));
        }
        warnings
    }

    /// Drops every edge the engines contributed and records why.
    ///
    /// An engine answering about bytes the served revision does not carry holds an
    /// older view of the whole tree, not of one location, so the edges it already gave
    /// for this read are dropped with the rest. The indexed edges answer, and
    /// the warning names the analysis that is missing from them. What the dropped
    /// answers said about callees and readiness leaves with them.
    fn degrade(&mut self, warning: ReadWarning) {
        self.incoming.clear();
        self.outgoing.clear();
        self.package_callees.clear();
        self.package_declarations.clear();
        self.dropped_callees = 0;
        self.unconfirmed.clear();
        self.analysis_unavailable = Some(warning);
    }

    #[cfg(test)]
    pub(crate) fn from_incoming(incoming: BTreeMap<CoreSymbolId, Vec<GraphHop>>) -> Self {
        Self {
            incoming,
            ..Self::default()
        }
    }

    #[cfg(test)]
    pub(crate) fn from_outgoing(outgoing: BTreeMap<CoreSymbolId, Vec<GraphHop>>) -> Self {
        Self {
            outgoing,
            ..Self::default()
        }
    }

    pub(crate) fn resolved(&self, symbol: &SymbolId) -> bool {
        self.incoming
            .keys()
            .chain(self.outgoing.keys())
            .any(|identity| identity.as_str() == symbol.0)
    }

    pub(crate) fn incoming(&self, symbol: &CoreSymbolId) -> &[GraphHop] {
        self.incoming.get(symbol).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn outgoing(&self, symbol: &CoreSymbolId) -> &[GraphHop] {
        self.outgoing.get(symbol).map_or(&[], Vec::as_slice)
    }

    /// The callees the walk found in package files, in the order it met them, each
    /// waiting for [`Self::name_package_callees`] or [`Self::drop_package_callees`].
    #[must_use]
    pub fn package_callees(&self) -> &[PackageCallee] {
        &self.package_callees
    }

    /// Ends each waiting callee's edge at the declaration `named` answers for it, with its
    /// kind and the exact package that holds it, and counts the callees `named` answers
    /// nothing for as dropped.
    ///
    /// A caller keeps one edge per callee declaration, whatever the number of calls the
    /// engine named for it. The walk does not continue from a package declaration: the
    /// local index holds no declaration of it to ask an engine about.
    pub fn name_package_callees(
        &mut self,
        mut named: impl FnMut(&PackageCallee) -> Option<CalleeDeclaration>,
    ) {
        for callee in std::mem::take(&mut self.package_callees) {
            let answered = named(&callee).and_then(|declaration| {
                let end = CoreSymbolId::new(declaration.id.0.clone()).ok()?;
                let held = PackageDeclaration {
                    symbol: callee.symbol(&declaration)?,
                    unit: callee.unit(&declaration.origin)?,
                };
                Some((declaration.id, end, held))
            });
            let Some((id, end, declaration)) = answered else {
                self.dropped_callees = self.dropped_callees.saturating_add(1);
                continue;
            };
            let caller = SymbolId(callee.caller().as_str().to_owned());
            let edges = self.outgoing.entry(callee.caller().clone()).or_default();
            if !edges.iter().any(|edge| edge.edge.to == id) {
                edges.push(call_hop(&caller, id, callee.edge_provider()));
                edges.sort_by(|left, right| left.edge.to.cmp(&right.edge.to));
            }
            self.package_declarations.entry(end).or_insert(declaration);
        }
    }

    /// Counts every waiting callee as dropped: the global API is off or did not answer.
    pub fn drop_package_callees(&mut self) {
        let waiting = u64::try_from(self.package_callees.len()).unwrap_or(u64::MAX);
        self.dropped_callees = self.dropped_callees.saturating_add(waiting);
        self.package_callees.clear();
    }

    /// The package declaration an outgoing edge ends at, `None` for a project one.
    pub(crate) fn package_declaration(&self, symbol: &CoreSymbolId) -> Option<&PackageDeclaration> {
        self.package_declarations.get(symbol)
    }

    pub(crate) fn validate_revision(&self, reads: &ReadService) -> Result<(), RiftError> {
        if self
            .revision
            .as_deref()
            .is_some_and(|revision| revision != reads.engine_revision())
        {
            return errors::server::read_unavailable()
                .operation("engine references")
                .detail("source revision changed before search")
                .fail();
        }
        Ok(())
    }
}

/// A package declaration an outgoing edge ends at: the symbol its hit carries, and the
/// package file holding it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PackageDeclaration {
    pub(crate) symbol: Symbol,
    pub(crate) unit: SourceUnitId,
}

/// Whether this search can request references from a configured language engine.
///
/// The indexed walk includes intermediate declarations that a depth-two traversal can
/// reach before its first engine request. No engine starts during this check.
///
/// # Errors
///
/// Returns the same request validation errors as the reference resolver.
pub fn uses_engine_references(
    reads: &ReadService,
    engines: &EnginePool,
    params: &SearchParams,
) -> Result<bool, RiftError> {
    reads.validate_engine_search(params)?;
    let Some((traversal, seed)) = reference_traversal(params) else {
        return Ok(false);
    };
    validate_traversal(traversal)?;
    let seed = CoreSymbolId::new(seed.0.clone()).map_err(|_| {
        errors::server::read_invalid()
            .field("traversal.seed")
            .violation("not a symbol identity")
            .error()
    })?;
    reachable_reference_source(reads.edges(), &seed, traversal, |identity| {
        Ok(reference_source(reads, engines, identity)?.is_some())
    })
}

fn reachable_reference_source(
    store: &EdgeStore,
    seed: &CoreSymbolId,
    traversal: &SearchTraversal,
    has_source: impl Fn(&CoreSymbolId) -> Result<bool, RiftError>,
) -> Result<bool, RiftError> {
    if has_source(seed)? {
        return Ok(true);
    }
    if traversal.depth == 1 {
        return Ok(false);
    }
    let mut prior = traversal.clone();
    prior.depth -= 1;
    let discovered = walk_traversal_with_references(
        store,
        seed,
        &prior,
        TRAVERSAL_SYMBOLS_MAX,
        &EngineReferences::default(),
    )
    .discovered;
    for (identity, _) in discovered {
        if has_source(&identity)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The traversal a configured engine may answer for, with the declaration it starts at:
/// references for an incoming walk, calls for an outgoing one. A walk riding beside
/// `change` names no `seed` and reaches no engine: the server refuses it before any engine
/// is asked.
fn reference_traversal(params: &SearchParams) -> Option<(&SearchTraversal, &SymbolId)> {
    let traversal = params.traversal.as_ref()?;
    let seed = traversal.seed.as_ref()?;
    let current = !crate::read::is_git_revision(params.revision.as_ref());
    let symbols = matches!(
        params.target,
        SearchParamsTarget::All | SearchParamsTarget::Symbol
    );
    let served =
        traversal.kinds.is_empty() || traversal.kinds.contains(&engine_kind(traversal.direction));
    (current && symbols && served).then_some((traversal, seed))
}

type ReferenceSource<'source> = (
    &'source EngineSlot,
    &'source IndexedFile,
    &'source SyntaxSymbol,
);

fn reference_source<'source>(
    reads: &'source ReadService,
    engines: &'source EnginePool,
    identity: &CoreSymbolId,
) -> Result<Option<ReferenceSource<'source>>, RiftError> {
    Ok(
        resolve_graph_symbol(reads.index(), identity)?.and_then(|(file, symbol)| {
            symbol.name_range?;
            let slot = engines.engine_for(file.syntax().language())?;
            Some((slot, file, symbol))
        }),
    )
}

/// Resolves incoming references or outgoing calls while retaining every indexed
/// edge.
///
/// Only current-tree symbol traversals that name no kind, or name the engine kind of
/// their direction, consult engines: `references` incoming, `calls` outgoing.
/// The traversal's existing depth and syntax element bounds also bound engine requests. Each
/// exchange uses the selected engine's retry and restart policy. The caller applies
/// its request deadline and validates the captured tree again before using the result.
/// Engines without references support leave the indexed answer unchanged.
///
/// An engine answering about bytes the served revision does not carry, or answering more
/// than the traversal bound carries, has its whole contribution dropped: the result holds
/// the indexed edges and an `engine_analysis_unavailable` warning naming what is
/// missing. No such condition refuses the read, because no resend of the same request
/// clears it.
///
/// A walk waits for each engine's readiness until `deadline`, in either direction; a
/// wait spent before the engine reads ready drops the engine's contribution with an
/// `engine_analysis_unavailable` warning and keeps the session loading.
///
/// An outgoing walk addresses each callee's file through `roots`. A callee in a package
/// file waits in [`EngineReferences::package_callees`] for the global API to name its
/// declaration, and the walk does not continue from it.
///
/// # Errors
///
/// Returns invalid traversal or engine failure, and refuses an outgoing seed the ready
/// engine prepares no call hierarchy item at, naming the seed's kind.
///
/// # Cancel safety
///
/// Dropping the future keeps the selected session while it reads intact, and the next
/// exchange on it closes the document this walk left open; a session with a frame cut
/// part-written is discarded. No source file is written.
pub async fn resolve_engine_references(
    reads: &ReadService,
    engines: &EnginePool,
    params: &SearchParams,
    (deadline, roots): (Instant, &CalleeRoots),
) -> Result<EngineReferences, RiftError> {
    if !uses_engine_references(reads, engines, params)? {
        return Ok(EngineReferences::default());
    }
    let Some((traversal, seed)) = reference_traversal(params) else {
        return Ok(EngineReferences::default());
    };
    validate_traversal(traversal)?;
    let seed = CoreSymbolId::new(seed.0.clone()).map_err(|_| {
        errors::server::read_invalid()
            .field("traversal.seed")
            .violation("not a symbol identity")
            .error()
    })?;
    let mut references = EngineReferences {
        revision: Some(reads.engine_revision().to_owned()),
        registry_unresolved: matches!(traversal.direction, TraversalDirection::Outgoing)
            && roots.registry_unresolved(),
        ..EngineReferences::default()
    };
    let mut pending = vec![seed.clone()];
    let mut requested = BTreeSet::new();
    for depth in 0..traversal.depth {
        let extended = match traversal.direction {
            TraversalDirection::Incoming => {
                Box::pin(extend_references(
                    reads,
                    engines,
                    &mut references,
                    pending,
                    &mut requested,
                    deadline,
                ))
                .await?
            }
            TraversalDirection::Outgoing => {
                let walk = OutgoingWalk {
                    seed: &seed,
                    deadline,
                    roots,
                };
                Box::pin(extend_callees(
                    reads,
                    engines,
                    &mut references,
                    pending,
                    &mut requested,
                    walk,
                ))
                .await?
            }
        };
        if let Some(warning) = extended {
            references.degrade(warning);
            return Ok(references);
        }
        if depth + 1 == traversal.depth {
            break;
        }
        let mut step = traversal.clone();
        step.depth = depth + 1;
        pending = walk_traversal_with_references(
            reads.edges(),
            &seed,
            &step,
            TRAVERSAL_SYMBOLS_MAX,
            &references,
        )
        .discovered
        .into_iter()
        .map(|(symbol, _)| symbol)
        .collect();
    }
    Ok(references)
}

async fn extend_references(
    reads: &ReadService,
    engines: &EnginePool,
    references: &mut EngineReferences,
    pending: Vec<CoreSymbolId>,
    requested: &mut BTreeSet<CoreSymbolId>,
    deadline: Instant,
) -> Result<Option<ReadWarning>, RiftError> {
    let stored: usize = references.incoming.values().map(Vec::len).sum();
    let mut remaining = TRAVERSAL_SYMBOLS_MAX - stored;
    for identity in pending {
        if !requested.insert(identity.clone()) {
            continue;
        }
        let resolved = Box::pin(resolve_symbol_references(
            reads, engines, &identity, deadline,
        ))
        .await?;
        let (edges, language) = match resolved {
            SymbolReferences::Resolved {
                edges,
                language,
                unconfirmed,
            } => {
                references.unconfirmed.extend(unconfirmed);
                (edges, language)
            }
            SymbolReferences::NotServed => continue,
            SymbolReferences::Unmapped { language, detail } => {
                return Ok(Some(engine_analysis_warning(language, detail)));
            }
            SymbolReferences::Unsettled { language, attempts } => {
                return Ok(Some(walk_unsettled_warning(language, attempts)));
            }
        };
        let Some(left) = remaining.checked_sub(edges.len()) else {
            return Ok(Some(engine_analysis_warning(
                language,
                format!(
                    "the engines answered more incoming references than the \
                     {TRAVERSAL_SYMBOLS_MAX}-symbol traversal bound carries"
                ),
            )));
        };
        remaining = left;
        references.incoming.insert(identity, edges);
    }
    if !references.unconfirmed.is_empty() {
        rift_tracing::info!(
            component = "engine",
            unconfirmed = ?references.unconfirmed,
            "incoming walk took unconfirmed answers"
        );
    }
    Ok(None)
}

/// What one seed's engine request contributed to a traversal.
enum SymbolReferences {
    /// The edges the engine resolved, possibly none, and the seed's language.
    Resolved {
        /// Incoming edges the engine named, mapped into the served tree.
        edges: Vec<GraphHop>,
        /// The seed declaration's language, the one whose engine answered.
        language: Language,
        /// The engine's name when the read took its answer unconfirmed.
        unconfirmed: Option<String>,
    },
    /// No engine serves this seed, or the one that does advertises no references
    /// capability. The indexed answer stands unchanged, and carries no warning: the
    /// caller asked for edges, not for an engine.
    NotServed,
    /// The engine answered about bytes the served revision does not carry, so nothing it
    /// said maps into the tree this read serves.
    Unmapped {
        /// The seed declaration's language, the one whose engine answered.
        language: Language,
        /// What did not fit the served bytes.
        detail: String,
    },
    /// The readiness wait was spent before the engine read ready.
    Unsettled {
        /// The seed declaration's language, the one whose engine is still analyzing.
        language: Language,
        /// Attempts the walk made before the wait was spent.
        attempts: u64,
    },
}

/// The warning a read carries once the engine tier's contribution is dropped.
fn engine_analysis_warning(language: Language, detail: impl Into<String>) -> ReadWarning {
    ReadWarning::EngineAnalysisUnavailable {
        language: Some(language),
        detail: detail.into(),
    }
}

/// The warning a walk carries when `[server] readiness_timeout` was spent
/// before the seed's engine read ready.
///
/// The engine's edges are missing from the answer, and the session stays live
/// and keeps loading, so a resend meets an engine further along.
fn walk_unsettled_warning(language: Language, attempts: u64) -> ReadWarning {
    let noun = if attempts == 1 { "attempt" } else { "attempts" };
    engine_analysis_warning(
        language,
        format!(
            "the language engine was still analyzing after {attempts} {noun} when \
             `[server] readiness_timeout` was spent, so this walk carries none of its \
             edges; resend the request once the engine reads ready"
        ),
    )
}

/// The warning an outgoing walk carries for the edges it dropped at callees it named no
/// declaration for.
fn callees_dropped_warning(callees: u64) -> ReadWarning {
    ReadWarning::CalleesDropped {
        callees,
        detail: "the language engine named callees outside the project and every installed \
                 package, or at a position the global index answered no declaration at, so \
                 the walk carries no edge to them"
            .to_owned(),
    }
}

/// The warning a walk carries when it took an answer from engines whose readiness was
/// unconfirmed, naming them.
fn readiness_unconfirmed_warning(processes: &BTreeSet<String>) -> ReadWarning {
    let named = processes
        .iter()
        .map(|process| format!("`{process}`"))
        .collect::<Vec<_>>()
        .join(", ");
    ReadWarning::EngineReadinessUnconfirmed {
        processes: processes.iter().cloned().collect(),
        detail: format!(
            "the walk took answers from {named} without progress evidence that the engine \
             had settled: it announced no work since it started or was last told of a \
             changed file, and stayed quiet past `settle_delay`"
        ),
    }
}

/// One outgoing edge per callee id.
///
/// Engine items that map to one declaration merge, so the walk reaches each
/// callee once. The embedded ty engine answers each overload stub of a
/// callee as its own item (`max` six times from `builtins.pyi`), and each
/// overload is its own declaration (`max`, `max~1`, and on), so overloads
/// stay apart. A call of the seed to itself draws no edge, as an incoming
/// reference from inside the seed draws none.
fn callee_hops(
    seed: &SymbolId,
    callees: impl IntoIterator<Item = SymbolId>,
    provider: EdgeProvider,
    mut corroborated: BTreeMap<SymbolId, SourceUnitSpan>,
) -> Vec<GraphHop> {
    callees
        .into_iter()
        .filter(|callee| callee != seed)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|callee| {
            let occurrence = corroborated.remove(&callee);
            let mut hop = call_hop(seed, callee, provider);
            hop.edge.occurrence = occurrence;
            hop
        })
        .collect()
}

/// The outgoing edge from `from` to the declaration `to` it calls.
fn call_hop(from: &SymbolId, to: SymbolId, provider: EdgeProvider) -> GraphHop {
    GraphHop {
        edge: graph_edge(from, to, EdgeKind::Calls, None, provider).into(),
        direction: HopDirection::Outgoing,
    }
}

/// Where one outgoing walk stands: the seed a refusal names, the instant each engine's
/// readiness wait ends, and the package roots its callees' files are addressed through.
#[derive(Clone, Copy)]
struct OutgoingWalk<'walk> {
    seed: &'walk CoreSymbolId,
    deadline: Instant,
    roots: &'walk CalleeRoots,
}

async fn extend_callees(
    reads: &ReadService,
    engines: &EnginePool,
    references: &mut EngineReferences,
    pending: Vec<CoreSymbolId>,
    requested: &mut BTreeSet<CoreSymbolId>,
    walk: OutgoingWalk<'_>,
) -> Result<Option<ReadWarning>, RiftError> {
    let stored: usize = references.outgoing.values().map(Vec::len).sum::<usize>()
        + references.package_callees.len();
    let mut remaining = TRAVERSAL_SYMBOLS_MAX - stored;
    for identity in pending {
        if !requested.insert(identity.clone()) {
            continue;
        }
        let resolved = resolve_symbol_callees(reads, engines, &identity, walk).await?;
        let (edges, held, language) = match resolved {
            SymbolCallees::Resolved {
                callees,
                corroborated,
                held,
                dropped,
                language,
                unconfirmed,
                provider,
            } => {
                references.dropped_callees = references.dropped_callees.saturating_add(dropped);
                references.unconfirmed.extend(unconfirmed);
                let seed = SymbolId(identity.as_str().to_owned());
                (
                    callee_hops(&seed, callees, provider, corroborated),
                    held,
                    language,
                )
            }
            SymbolCallees::NotServed => continue,
            SymbolCallees::Unprepared { kind } if identity == *walk.seed => {
                return errors::server::read_unsupported()
                    .capability(format!(
                        "outgoing traversal from a declaration of kind `{kind}` (the language \
                     engine prepares no call hierarchy item at the seed)"
                    ))
                    .fail();
            }
            SymbolCallees::Unprepared { .. } => {
                references.outgoing.insert(identity, Vec::new());
                continue;
            }
            SymbolCallees::Unmapped { language, detail } => {
                return Ok(Some(engine_analysis_warning(language, detail)));
            }
            SymbolCallees::Unsettled { language, attempts } => {
                return Ok(Some(walk_unsettled_warning(language, attempts)));
            }
        };
        let Some(left) = remaining.checked_sub(edges.len() + held.len()) else {
            return Ok(Some(engine_analysis_warning(
                language,
                format!(
                    "the engines answered more outgoing calls than the \
                     {TRAVERSAL_SYMBOLS_MAX}-symbol traversal bound carries"
                ),
            )));
        };
        remaining = left;
        references.outgoing.insert(identity, edges);
        references.package_callees.extend(held);
    }
    if !references.unconfirmed.is_empty() || references.dropped_callees > 0 {
        rift_tracing::info!(
            component = "engine",
            unconfirmed = ?references.unconfirmed,
            dropped_callees = references.dropped_callees,
            "outgoing walk took unconfirmed answers or dropped callees outside every root"
        );
    }
    Ok(None)
}

/// What one declaration's outgoing calls request contributed to a walk.
enum SymbolCallees {
    /// The callees the engine named inside this tree and in package files, and the count
    /// it named under no root.
    Resolved {
        /// The analysis selected for the declaration.
        provider: EdgeProvider,
        /// Callee declarations, one per call the engine named, repeats included.
        callees: Vec<SymbolId>,
        /// Exact stored occurrences independently reported at the selected caller.
        corroborated: BTreeMap<SymbolId, SourceUnitSpan>,
        /// Callees in package files, waiting for the global API to name them.
        held: Vec<PackageCallee>,
        /// Callees under no root, or outside every declaration of a project file.
        dropped: u64,
        /// The declaration's language, the one whose engine answered.
        language: Language,
        /// The engine's name when the walk took its answer unconfirmed.
        unconfirmed: Option<String>,
    },
    /// No engine serves this declaration, or the one that does advertises no call
    /// hierarchy.
    NotServed,
    /// The ready engine prepared no call hierarchy item at the declaration.
    Unprepared {
        /// The declaration's kind, which a refused seed names.
        kind: &'static str,
    },
    /// The engine answered about bytes the served revision does not carry.
    Unmapped {
        /// The declaration's language, the one whose engine answered.
        language: Language,
        /// What did not fit the served bytes.
        detail: String,
    },
    /// The readiness wait was spent before the engine read ready.
    Unsettled {
        /// The declaration's language, the one whose engine is still analyzing.
        language: Language,
        /// Attempts the walk made before the wait was spent.
        attempts: u64,
    },
}

async fn resolve_symbol_callees(
    reads: &ReadService,
    engines: &EnginePool,
    identity: &CoreSymbolId,
    walk: OutgoingWalk<'_>,
) -> Result<SymbolCallees, RiftError> {
    let Some((slot, file, symbol)) = reference_source(reads, engines, identity)? else {
        return Ok(SymbolCallees::NotServed);
    };
    let language = file.syntax().language().clone();
    let target = ReferenceTarget::new(slot.workspace_root(), reads.index().root(), file, symbol)?;
    let answered = Box::pin(calls_on_engine(
        slot,
        &target,
        TraversalDirection::Outgoing,
        false,
        walk.deadline,
    ))
    .await;
    let (report, unconfirmed) = match answered {
        Ok(OutgoingAnswer::Ready(report)) => (report, None),
        Ok(OutgoingAnswer::Unconfirmed(report)) => (report, Some(slot.name().to_owned())),
        Ok(OutgoingAnswer::Unprepared) => {
            return Ok(SymbolCallees::Unprepared { kind: symbol.kind });
        }
        Ok(OutgoingAnswer::Unsettled { attempts }) => {
            return Ok(SymbolCallees::Unsettled { language, attempts });
        }
        Err(error) if error.slug() == errors::lsp::engine_capability_absent::SLUG => {
            return Ok(SymbolCallees::NotServed);
        }
        Err(error) => return error.fail(),
    };
    let trees = [slot.workspace_root(), reads.index().root()];
    match map_callees(
        reads,
        report,
        identity,
        (walk.roots, &trees),
        slot.edge_provider(),
        Some(file),
    ) {
        Ok(MappedCallees {
            callees,
            corroborated,
            held,
            dropped,
        }) => Ok(SymbolCallees::Resolved {
            provider: slot.edge_provider(),
            callees,
            corroborated,
            held,
            dropped,
            language,
            unconfirmed,
        }),
        Err(error) if error.slug() == errors::server::read_engine_answer::SLUG => {
            Ok(SymbolCallees::Unmapped {
                language,
                detail: error.detail(),
            })
        }
        Err(error) => error.fail(),
    }
}

/// One outgoing calls answer: each callee the engine named, or the count past the
/// traversal bound.
struct CalleeReport {
    calls: Result<Vec<CallHierarchyOutgoingCall>, usize>,
    encoding: PositionEncoding,
}

async fn calls_on_engine(
    slot: &EngineSlot,
    target: &ReferenceTarget,
    direction: TraversalDirection,
    exact_seed: bool,
    deadline: Instant,
) -> Result<OutgoingAnswer<CalleeReport>, RiftError> {
    let request_path = target.path.clone();
    let utf8 = target.utf8;
    let utf16 = target.utf16;
    let uri = target.uri.clone();
    let canonical_uri = target.canonical_uri.clone();
    slot.request_outgoing(
        target.opening(),
        move |session: &mut EngineSession| {
            let path = request_path.clone();
            let uri = uri.clone();
            let canonical_uri = canonical_uri.clone();
            Box::pin(async move {
                let encoding = session.capabilities().position_encoding;
                let position = match encoding {
                    PositionEncoding::Utf8 => utf8,
                    PositionEncoding::Utf16 => utf16,
                };
                let items = session.prepare_call_hierarchy(&path, position).await?;
                if items.is_empty() {
                    return Ok(None);
                }
                if items.len() > TRAVERSAL_SYMBOLS_MAX {
                    return Ok(Some(CalleeReport {
                        calls: Err(items.len()),
                        encoding,
                    }));
                }
                let mut calls = Vec::new();
                let mut exact_caller = true;
                for item in items {
                    let selected = (item.uri == uri || item.uri == canonical_uri)
                        && item.selection_range.start <= position
                        && position < item.selection_range.end;
                    if exact_seed && !selected {
                        return Err(graph_call_coverage());
                    }
                    exact_caller &= selected;
                    match direction {
                        TraversalDirection::Outgoing => {
                            calls.extend(session.outgoing_calls(item).await?);
                        }
                        TraversalDirection::Incoming => {
                            calls.extend(edge_incoming_calls(session, item).await?);
                        }
                    }
                    if calls.len() > TRAVERSAL_SYMBOLS_MAX {
                        return Ok(Some(CalleeReport {
                            calls: Err(calls.len()),
                            encoding,
                        }));
                    }
                }
                // The legacy walk can use neighbors from a prepared item outside the
                // selected declaration. Its ranges cannot verify occurrences in this file.
                if !exact_caller {
                    for call in &mut calls {
                        call.from_ranges.clear();
                    }
                }
                Ok(Some(CalleeReport {
                    calls: Ok(calls),
                    encoding,
                }))
            })
        },
        target.closing(),
        deadline,
    )
    .await
}

async fn edge_incoming_calls(
    session: &mut EngineSession,
    item: lsp_types::CallHierarchyItem,
) -> Result<Vec<CallHierarchyOutgoingCall>, RiftError> {
    let incoming = session.incoming_calls(item.clone()).await?;
    if incoming.len() > TRAVERSAL_SYMBOLS_MAX {
        return Err(graph_call_coverage());
    }
    let mut calls = Vec::new();
    let mut remaining = TRAVERSAL_SYMBOLS_MAX;
    for call in incoming {
        remaining = remaining
            .checked_sub(1 + call.from_ranges.len())
            .ok_or_else(graph_call_coverage)?;
        let outgoing = session.outgoing_calls(call.from.clone()).await?;
        remaining = remaining
            .checked_sub(outgoing.len())
            .ok_or_else(graph_call_coverage)?;
        for edge in &outgoing {
            remaining = remaining
                .checked_sub(edge.from_ranges.len())
                .ok_or_else(graph_call_coverage)?;
        }
        let matching = outgoing
            .iter()
            .filter(|outgoing| edge_same_callee(&item, &outgoing.to))
            .collect::<Vec<_>>();
        if matching.is_empty() {
            continue;
        }
        let incoming_unlocated = call.from_ranges.is_empty();
        let outgoing_unlocated = matching
            .iter()
            .any(|outgoing| outgoing.from_ranges.is_empty());
        let ranges: Vec<_> = call
            .from_ranges
            .into_iter()
            .filter(|incoming| {
                outgoing_unlocated
                    || matching.iter().any(|outgoing| {
                        outgoing
                            .from_ranges
                            .iter()
                            .any(|outgoing| edge_call_ranges_overlap(incoming, outgoing))
                    })
            })
            .collect();
        if incoming_unlocated || !ranges.is_empty() {
            calls.push(CallHierarchyOutgoingCall {
                to: call.from,
                from_ranges: ranges,
            });
        }
    }
    Ok(calls)
}

fn edge_same_callee(
    left: &lsp_types::CallHierarchyItem,
    right: &lsp_types::CallHierarchyItem,
) -> bool {
    left.uri == right.uri && left.selection_range == right.selection_range
}

fn edge_call_ranges_overlap(left: &Range, right: &Range) -> bool {
    left.start < right.end && right.start < left.end
}

/// What one outgoing calls answer maps to.
struct MappedCallees {
    /// The project declarations holding each callee's name, one per call.
    callees: Vec<SymbolId>,
    corroborated: BTreeMap<SymbolId, SourceUnitSpan>,
    /// The callees in package files, one per call.
    held: Vec<PackageCallee>,
    /// The callees under no root, or outside every declaration of a project file.
    dropped: u64,
}

/// Maps each callee the engine named: a project file's callee to the declaration holding
/// its name, and a package file's callee, or a typeshed stub's, to the position the global
/// API names its declaration at. A callee under no root, or outside every declaration of
/// its project file, is counted.
fn map_callees(
    reads: &ReadService,
    report: CalleeReport,
    caller: &CoreSymbolId,
    (roots, trees): (&CalleeRoots, &[&std::path::Path]),
    provider: EdgeProvider,
    caller_file: Option<&IndexedFile>,
) -> Result<MappedCallees, RiftError> {
    let calls =
        report.calls.map_err(|observed| {
            errors::server::read_engine_answer().operation("engine calls").detail(format!(
                "outgoing calls {observed} exceed the {TRAVERSAL_SYMBOLS_MAX}-symbol traversal bound"
            )).error()
        })?;
    let mut mapped = MappedCallees {
        callees: Vec::new(),
        corroborated: BTreeMap::new(),
        held: Vec::new(),
        dropped: 0,
    };
    let mut observations = caller_file.map(|file| WalkCallSites::new(file, report.encoding));
    for call in &calls {
        let item = &call.to;
        let named = NamedCallee {
            provider,
            uri: &item.uri,
            name: &item.name,
            position: item.selection_range.start,
            encoding: report.encoding,
        };
        let file = callee_file(roots, trees, caller, named).map_err(|error| {
            errors::server::read_task()
                .operation("callee URI conversion")
                .detail(error.detail())
                .error()
        })?;
        let declaration = match file {
            CalleeFile::Project(path) => project_declaration(
                reads,
                &path,
                (item.selection_range, report.encoding),
                "engine calls",
            )?,
            CalleeFile::Package(held) => {
                if let Some(sites) = &mut observations {
                    sites.record(&call.from_ranges, None);
                }
                mapped.held.push(held);
                continue;
            }
            CalleeFile::Unaddressed => None,
        };
        if let Some(sites) = &mut observations {
            sites.record(&call.from_ranges, declaration.as_ref());
        }
        match declaration {
            Some(declaration) => mapped.callees.push(declaration),
            None => mapped.dropped += 1,
        }
    }
    if let Some(sites) = observations {
        mapped.corroborated = sites.corroborate(reads.edges().outgoing(caller));
    }
    Ok(mapped)
}

/// Location evidence from one prepared caller, independent of traversal reachability.
struct WalkCallSites<'source> {
    text: &'source str,
    index: Option<LineIndex<'source>>,
    unit: SourceUnitId,
    encoding: PositionEncoding,
    remaining: Option<usize>,
    targets: BTreeMap<(u64, u64), Option<SymbolId>>,
}

impl<'source> WalkCallSites<'source> {
    fn new(file: &'source IndexedFile, encoding: PositionEncoding) -> Self {
        Self {
            text: file.source(),
            index: None,
            unit: crate::read::source_unit_id(file.path()),
            encoding,
            remaining: Some(TRAVERSAL_SYMBOLS_MAX),
            targets: BTreeMap::new(),
        }
    }

    fn record(&mut self, ranges: &[Range], target: Option<&SymbolId>) {
        if ranges.is_empty() {
            return;
        }
        self.remaining = self
            .remaining
            .and_then(|remaining| remaining.checked_sub(ranges.len()));
        if self.remaining.is_none() {
            return;
        }
        let index = self.index.get_or_insert_with(|| LineIndex::new(self.text));
        for range in ranges {
            let (Ok(start), Ok(end)) = (
                index.byte_offset(self.encoding, range.start),
                index.byte_offset(self.encoding, range.end),
            ) else {
                self.remaining = None;
                return;
            };
            if start >= end {
                continue;
            }
            // Repeated reports of the same target agree. A different or unresolved
            // target at these bytes cannot independently verify a stored attribution.
            self.targets
                .entry((start as u64, end as u64))
                .and_modify(|prior| {
                    if prior.as_ref() != target {
                        *prior = None;
                    }
                })
                .or_insert_with(|| target.cloned());
        }
    }

    fn corroborate(self, outgoing: &[rift_index::Edge]) -> BTreeMap<SymbolId, SourceUnitSpan> {
        let mut corroborated = BTreeMap::new();
        if self.remaining.is_none() || self.targets.is_empty() {
            return corroborated;
        }
        let mut seen = BTreeSet::new();
        // The walk uses the first stored edge of this kind for a neighbor.
        for edge in outgoing.iter().take(TRAVERSAL_SYMBOLS_MAX) {
            if edge.kind() != EdgeKind::Calls || !seen.insert(edge.to()) {
                continue;
            }
            let span = edge.occurrence();
            let (start, end) = (span.range().start(), span.range().end());
            if self
                .targets
                .get(&(start, end))
                .and_then(Option::as_ref)
                .is_some_and(|target| target.as_str() == edge.to().as_str())
                && span.unit().to_string() == self.unit.0
            {
                corroborated.insert(
                    SymbolId(edge.to().as_str().to_owned()),
                    SourceUnitSpan {
                        unit: self.unit.clone(),
                        range: TextRange { start, end },
                    },
                );
            }
        }
        corroborated
    }
}

mod call_evidence;
mod heritage_edges;
mod type_uses;
use call_evidence::CapturedCalls;
pub use heritage_edges::{graph_heritage_edges, graph_heritage_inventory};
pub use type_uses::graph_type_uses;

/// Calls established for one selected local declaration.
#[derive(Clone, Debug, Default)]
pub struct GraphCalls {
    /// Canonical neighbors in the current tree.
    pub local: Vec<(SymbolId, Vec<SourceUnitSpan>)>,
    /// Original edges with their analysis and physical occurrences.
    pub edges: Vec<StoredEdge>,
    /// Outgoing targets in admitted dependency sources, awaiting exact identity resolution.
    pub packages: Vec<(PackageCallee, Vec<SourceUnitSpan>)>,
    /// Whether static capture or a configured engine covered the requested direction.
    pub complete: bool,
    /// Original observations linked to a captured authored call site.
    pub corroboration: Vec<CallCorroboration>,
    /// Typed warnings from the engine edge read.
    pub edge_warnings: Vec<ReadWarning>,
}

impl GraphCalls {
    /// Number of retained call targets, occurrences, and observations.
    #[must_use]
    pub fn record_count(&self) -> usize {
        self.local
            .iter()
            .map(|(_, ranges)| ranges.len().saturating_add(1))
            .chain(
                self.packages
                    .iter()
                    .map(|(_, ranges)| ranges.len().saturating_add(1)),
            )
            .fold(self.corroboration.len(), usize::saturating_add)
    }

    /// Adds calls and keeps both answers' coverage and observations.
    pub fn merge(&mut self, other: Self) {
        self.complete &= other.complete;
        self.edges.extend(other.edges);
        self.local.extend(other.local);
        self.packages.extend(other.packages);
        self.corroboration.extend(other.corroboration);
        self.edge_warnings.extend(other.edge_warnings);
    }

    /// Names a package call with the analysis that produced it.
    pub fn name_package_call(
        &mut self,
        callee: &PackageCallee,
        target: SymbolId,
        occurrences: Vec<SourceUnitSpan>,
    ) {
        let seed = SymbolId(callee.caller().as_str().to_owned());
        self.push_local(
            &seed,
            target,
            occurrences,
            TraversalDirection::Outgoing,
            callee.edge_provider(),
        );
    }

    fn push_local(
        &mut self,
        seed: &SymbolId,
        neighbor: SymbolId,
        occurrences: Vec<SourceUnitSpan>,
        direction: TraversalDirection,
        provider: EdgeProvider,
    ) {
        let (from, to) = match direction {
            TraversalDirection::Outgoing => (seed, &neighbor),
            TraversalDirection::Incoming => (&neighbor, seed),
        };
        if occurrences.is_empty() {
            self.edges.push(graph_edge(
                from,
                to.clone(),
                EdgeKind::Calls,
                None,
                provider,
            ));
        } else {
            self.edges.extend(occurrences.iter().map(|span| {
                graph_edge(
                    from,
                    to.clone(),
                    EdgeKind::Calls,
                    Some(span.clone()),
                    provider,
                )
            }));
        }
        self.local.push((neighbor, occurrences));
    }
}

fn graph_edge(
    from: &SymbolId,
    to: SymbolId,
    kind: EdgeKind,
    occurrence: Option<SourceUnitSpan>,
    provider: EdgeProvider,
) -> StoredEdge {
    StoredEdge {
        from: from.clone(),
        to,
        kind: ExactKind(kind.as_ref().to_owned()),
        kinds: vec![kind],
        occurrence,
        provider,
        derivation: EdgeDerivation::Resolution,
        confidence: None,
        producer: None,
        extensions: Extensions::default(),
    }
}

/// An engine's original destination before projection into canonical adjacency.
#[derive(Clone, Debug, PartialEq)]
pub enum CallDestination {
    /// Canonical local declaration.
    Local(SymbolId),
    /// Address in an admitted dependency's source.
    Dependency(Box<PackageCallee>),
}

/// Provider that supplied an independently observed call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallProvider {
    /// Configured language engine.
    Engine,
    /// A configured engine's incoming references at the authored callee.
    EngineReferences,
    /// Language-specific extractor.
    Extractor,
}

/// Independent call evidence attached to the same authored site.
#[derive(Clone, Debug)]
pub struct CallCorroboration {
    /// Invocation span supplied by static resolution.
    pub authored: SourceUnitSpan,
    /// Authored callee selection, excluding invocation arguments.
    pub callee: TextRange,
    /// Original engine span.
    pub observed: SourceUnitSpan,
    /// Target selected by static resolution.
    pub static_target: SymbolId,
    /// Original engine target.
    pub engine_target: CallDestination,
    /// Origin of the observation combined with the static call.
    pub provider: CallProvider,
    /// Whether the evidence establishes the same target.
    pub agrees: bool,
}

/// Reads every adjacent call and its physical call sites in the selected direction.
///
/// # Errors
/// Refuses missing readiness, an unaddressed callee, or work outside the accepted bounds.
pub async fn graph_calls(
    reads: &ReadService,
    engines: &EnginePool,
    roots: &CalleeRoots,
    seed: &SymbolId,
    direction: TraversalDirection,
    deadline: Instant,
) -> Result<GraphCalls, RiftError> {
    let unavailable = graph_call_coverage;
    let identity = CoreSymbolId::new(seed.as_str().to_owned()).map_err(|_| unavailable())?;
    let declarations = edge_declarations(reads, &identity)?;
    if declarations.is_empty() {
        return Err(unavailable());
    }
    if direction == TraversalDirection::Outgoing
        && declarations.iter().any(|declaration| {
            engines
                .engine_for(declaration.file.syntax().language())
                .is_some()
        })
    {
        return Box::pin(inventory::outgoing_calls(
            reads, engines, roots, seed, deadline,
        ))
        .await;
    }
    let static_calls = CapturedCalls::read(reads, &identity, direction);
    let mut combined = GraphCalls {
        complete: true,
        ..GraphCalls::default()
    };
    let mut observed = false;
    let mut remaining = TRAVERSAL_SYMBOLS_MAX;
    for declaration in declarations {
        let answer = Box::pin(graph_calls_for_declaration(
            reads,
            engines,
            roots,
            &identity,
            declaration,
            direction,
            deadline,
        ))
        .await?;
        let Some(answer) = answer else {
            combined.complete &= static_calls.is_complete();
            continue;
        };
        remaining = remaining
            .checked_sub(answer.record_count())
            .ok_or_else(unavailable)?;
        observed = true;
        combined.complete &= answer.complete;
        combined.edges.extend(answer.edges);
        combined.local.extend(answer.local);
        combined.packages.extend(answer.packages);
        combined.edge_warnings.extend(answer.edge_warnings);
        combined.corroboration.extend(answer.corroboration);
    }
    let answer = if observed {
        static_calls.combine(combined)
    } else {
        static_calls.answer().ok_or_else(unavailable)?
    };
    if direction == TraversalDirection::Outgoing {
        Ok(Box::pin(call_evidence::confirm_reference_sites(
            reads,
            engines,
            seed,
            answer,
            &mut call_evidence::TargetReferences::default(),
            deadline,
        ))
        .await)
    } else {
        Ok(answer)
    }
}

async fn graph_calls_for_declaration(
    reads: &ReadService,
    engines: &EnginePool,
    roots: &CalleeRoots,
    identity: &CoreSymbolId,
    declaration: rift_index::SymbolMatch<'_>,
    direction: TraversalDirection,
    deadline: Instant,
) -> Result<Option<GraphCalls>, RiftError> {
    let file = declaration.file;
    let Some(slot) = engines.engine_for(file.syntax().language()) else {
        return Ok(None);
    };
    let request = ReferenceTarget::new(
        slot.workspace_root(),
        reads.index().root(),
        file,
        declaration.symbol,
    )?;
    let answered = match Box::pin(calls_on_engine(slot, &request, direction, true, deadline)).await
    {
        Ok(answered) => answered,
        Err(error)
            if graph_engine_method_missing(&error)
                || error.slug() == errors::server::search_coverage_unavailable::SLUG =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let (report, edge_warning) = match answered {
        OutgoingAnswer::Ready(report) => (report, None),
        OutgoingAnswer::Unconfirmed(report) => (
            report,
            Some(readiness_unconfirmed_warning(&BTreeSet::from([slot
                .name()
                .to_owned()]))),
        ),
        OutgoingAnswer::Unsettled { attempts } => {
            let mut answer = GraphCalls::default();
            answer.edge_warnings.push(walk_unsettled_warning(
                file.syntax().language().clone(),
                attempts,
            ));
            return Ok(Some(answer));
        }
        OutgoingAnswer::Unprepared => return Ok(None),
    };
    let engine = map_graph_calls(
        reads,
        roots,
        identity,
        report,
        &CallGraphSource {
            provider: slot.edge_provider(),
            trees: [slot.workspace_root(), reads.index().root()],
            source: file.source(),
            unit: crate::read::source_unit_id(file.path()),
            direction,
        },
    );
    match engine {
        Ok(mut engine) => {
            engine.edge_warnings.extend(edge_warning);
            Ok(Some(engine))
        }
        Err(error) if error.slug() == errors::server::search_coverage_unavailable::SLUG => Ok(None),
        Err(error) => Err(error),
    }
}

/// Reads the calls the captured static pass established for one declaration, without a
/// language engine. `complete` holds only when static capture covers the declaration.
///
/// # Errors
/// Refuses a seed that is not a canonical identity.
pub fn captured_graph_calls(
    reads: &ReadService,
    seed: &SymbolId,
    direction: TraversalDirection,
) -> Result<GraphCalls, RiftError> {
    let identity =
        CoreSymbolId::new(seed.as_str().to_owned()).map_err(|_| graph_call_coverage())?;
    Ok(CapturedCalls::read(reads, &identity, direction)
        .answer()
        .unwrap_or_default())
}

struct CallGraphSource<'source> {
    trees: [&'source std::path::Path; 2],
    source: &'source str,
    unit: SourceUnitId,
    direction: TraversalDirection,
    provider: EdgeProvider,
}

fn map_graph_calls(
    reads: &ReadService,
    roots: &CalleeRoots,
    identity: &CoreSymbolId,
    report: CalleeReport,
    held: &CallGraphSource<'_>,
) -> Result<GraphCalls, RiftError> {
    let unavailable = graph_call_coverage;
    let mut result = GraphCalls {
        complete: true,
        ..GraphCalls::default()
    };
    let seed = SymbolId(identity.as_str().to_owned());
    let mut locations = 0usize;
    let captured = held
        .unit
        .project_path()
        .map(ProjectPath::new)
        .transpose()?
        .and_then(|path| reads.index().file(&path));
    let outgoing = if held.direction == TraversalDirection::Outgoing {
        captured.map(|file| (file, identity))
    } else {
        None
    };
    for call in report.calls.map_err(|_| unavailable())? {
        let item = &call.to;
        let at = NamedCallee {
            provider: held.provider,
            uri: &item.uri,
            name: &item.name,
            position: item.selection_range.start,
            encoding: report.encoding,
        };
        let neighbor = callee_file(roots, &held.trees, identity, at)?;
        let (source, unit, incoming_file) = match (&neighbor, held.direction) {
            (_, TraversalDirection::Outgoing) => (held.source, held.unit.clone(), None),
            (CalleeFile::Project(path), TraversalDirection::Incoming) => {
                let Some(file) = reads.index().file(path) else {
                    result.complete = false;
                    continue;
                };
                (
                    file.source(),
                    crate::read::source_unit_id(file.path()),
                    Some(file),
                )
            }
            _ => {
                result.complete = false;
                continue;
            }
        };
        let index = LineIndex::new(source);
        let located = !call.from_ranges.is_empty();
        let occurrences = graph_call_site_spans(
            reads,
            (source, &unit, &index),
            (report.encoding, call.from_ranges),
            outgoing,
            &mut locations,
        )?;
        if located && occurrences.is_empty() {
            continue;
        }
        if let Some(file) = incoming_file
            && located
        {
            let range = item.selection_range;
            let start = index.byte_offset(report.encoding, range.start)? as u64;
            let end = index.byte_offset(report.encoding, range.end)? as u64;
            if start > end {
                return Err(unavailable());
            }
            place_incoming_call_sites(
                reads,
                file,
                (start, end),
                &seed,
                held.provider,
                occurrences,
                &mut result,
            )?;
            continue;
        }
        push_reported_graph_call(
            reads,
            neighbor,
            &seed,
            (item.selection_range, report.encoding),
            held,
            occurrences,
            &mut result,
        )?;
    }
    Ok(result)
}

fn push_reported_graph_call(
    reads: &ReadService,
    neighbor: CalleeFile,
    seed: &SymbolId,
    selection: (Range, PositionEncoding),
    held: &CallGraphSource<'_>,
    occurrences: Vec<SourceUnitSpan>,
    result: &mut GraphCalls,
) -> Result<(), RiftError> {
    match neighbor {
        CalleeFile::Project(path) => {
            let target = match held.direction {
                TraversalDirection::Outgoing => project_definition(reads, &path, selection, false)?,
                TraversalDirection::Incoming => {
                    project_declaration(reads, &path, selection, "graph calls")?
                }
            };
            if let Some(target) = target {
                result.push_local(seed, target, occurrences, held.direction, held.provider);
            } else {
                result.complete = false;
            }
        }
        CalleeFile::Package(target) => result.packages.push((target, occurrences)),
        CalleeFile::Unaddressed => result.complete = false,
    }
    Ok(())
}

/// Converts provider locations once and keeps the report-wide location ceiling.
fn graph_call_site_spans(
    reads: &ReadService,
    (source, unit, index): (&str, &SourceUnitId, &LineIndex),
    (encoding, ranges): (PositionEncoding, Vec<Range>),
    outgoing: Option<(&IndexedFile, &CoreSymbolId)>,
    locations: &mut usize,
) -> Result<Vec<SourceUnitSpan>, RiftError> {
    let mut occurrences = Vec::new();
    for range in ranges {
        *locations = locations
            .checked_add(1)
            .filter(|count| *count <= TRAVERSAL_SYMBOLS_MAX)
            .ok_or_else(graph_call_coverage)?;
        let start = index.byte_offset(encoding, range.start)?;
        let end = index.byte_offset(encoding, range.end)?;
        if source.get(start..end).is_none() {
            return Err(graph_call_coverage());
        }
        if let Some((file, identity)) = outgoing
            && edge_enclosing_symbol(reads, file, start as u64, end as u64)?
                .is_some_and(|caller| caller.as_str() != identity.as_str())
        {
            continue;
        }
        occurrences.push(SourceUnitSpan {
            unit: unit.clone(),
            range: TextRange {
                start: start as u64,
                end: end as u64,
            },
        });
    }
    Ok(occurrences)
}

fn place_incoming_call_sites(
    reads: &ReadService,
    file: &IndexedFile,
    (selection_start, selection_end): (u64, u64),
    seed: &SymbolId,
    provider: EdgeProvider,
    occurrences: Vec<SourceUnitSpan>,
    result: &mut GraphCalls,
) -> Result<(), RiftError> {
    let direction = TraversalDirection::Incoming;
    let Some(reported) = file.enclosing_symbol(selection_start, selection_end) else {
        result.complete = false;
        return Ok(());
    };
    let Some(reported_id) = symbol_id(reads.index(), crate::search::declared(file, reported))?
    else {
        result.complete = false;
        return Ok(());
    };
    let mut callers = BTreeMap::<_, Vec<_>>::new();
    for occurrence in occurrences {
        let Some(scope) = incoming_call_scope(file, reported, &occurrence.range) else {
            result.complete = false;
            continue;
        };
        let caller = if !std::ptr::eq(scope, reported)
            && scope
                .kinds
                .contains(&rift_protocol::read::SymbolKind::Callable)
        {
            if scope.name_range.is_none() {
                result.complete = false;
                continue;
            }
            symbol_id(reads.index(), crate::search::declared(file, scope))?
        } else {
            // Static call evidence can establish a non-callable initializer as the caller.
            Some(reported_id.clone())
        };
        let Some(caller) = caller else {
            result.complete = false;
            continue;
        };
        callers.entry(caller).or_default().push(occurrence);
    }
    for (caller, occurrences) in callers {
        result.push_local(seed, caller, occurrences, direction, provider);
    }
    Ok(())
}

fn incoming_call_scope<'file>(
    file: &'file IndexedFile,
    reported: &SyntaxSymbol,
    range: &TextRange,
) -> Option<&'file SyntaxSymbol> {
    if range.start >= range.end {
        return None;
    }
    // An engine can group sites from a nested callable into its parent's item.
    // The same captured declaration must hold both ends of the call range.
    let first = file.enclosing_symbol(range.start, range.start + 1)?;
    let last = file.enclosing_symbol(range.end - 1, range.end)?;
    (std::ptr::eq(first, last)
        && first.item_range.start <= range.start
        && range.end <= first.item_range.end
        && (std::ptr::eq(first, reported)
            || first.item_range != reported.item_range
                && reported.item_range.encloses(first.item_range)))
    .then_some(first)
}

fn graph_engine_method_missing(error: &RiftError) -> bool {
    error.slug() == errors::lsp::engine_capability_absent::SLUG
        || (error.slug() == errors::lsp::engine_refused_terminal::SLUG
            && error.context().any(|(key, value)| {
                key == "code"
                    && value.parse::<i64>() == Ok(rift_lsp::correlation::METHOD_NOT_FOUND_CODE)
            }))
}

fn graph_call_coverage() -> RiftError {
    errors::server::search_coverage_unavailable()
        .operation("graph calls")
        .error()
}

/// Reprepares and reads a call target from the same admitted installed bytes.
///
/// # Errors
/// Refuses missing readiness, a different prepared position or an unaddressed callee.
pub async fn installed_graph_calls(
    reads: &ReadService,
    engines: &EnginePool,
    roots: &CalleeRoots,
    seed: &SymbolId,
    target: (&PackageCallee, &Language, &SourceUnitId, &str),
    deadline: Instant,
) -> Result<GraphCalls, RiftError> {
    let (callee, language, unit, text) = target;
    let slot = engines
        .engine_for(language)
        .ok_or_else(graph_call_coverage)?;
    let uri = callee.document().ok_or_else(graph_call_coverage)?.clone();
    let offset = callee.byte_offset(text)?;
    let index = LineIndex::new(text);
    let utf8 = index.position(PositionEncoding::Utf8, offset)?;
    let utf16 = index.position(PositionEncoding::Utf16, offset)?;
    let source: std::sync::Arc<str> = text.into();
    let open_uri = uri.clone();
    let close_uri = uri.clone();
    let language_id = language.name.clone();
    let result = slot.request_outgoing(
        move |session| {
            let uri = open_uri.clone();
            let source = std::sync::Arc::clone(&source);
            let language_id = language_id.clone();
            Box::pin(async move { session.open_uri(uri, &language_id, source.to_string()).await })
        },
        move |session| {
            let uri = uri.clone();
            Box::pin(async move {
                let encoding = session.capabilities().position_encoding;
                let position = match encoding {
                    PositionEncoding::Utf8 => utf8,
                    PositionEncoding::Utf16 => utf16,
                };
                let items = session.prepare_call_hierarchy_uri(uri.clone(), position).await?;
                let [item] = items.as_slice() else {
                    return Err(graph_call_coverage());
                };
                if item.uri != uri || !(item.selection_range.start <= position && position < item.selection_range.end) {
                    return Err(graph_call_coverage());
                }
                let calls = session.outgoing_calls(item.clone()).await?;
                if calls.len() > TRAVERSAL_SYMBOLS_MAX {
                    return Err(graph_call_coverage());
                }
                Ok(Some(CalleeReport { calls: Ok(calls), encoding }))
            })
        },
        move |session| {
            let uri = close_uri.clone();
            Box::pin(async move {
                if let Err(error) = session.close_uri(&uri).await {
                    rift_tracing::warn!(component = "engine", error = %error, "engine document close failed");
                }
            })
        },
        deadline,
    ).await?;
    let OutgoingAnswer::Ready(report) = result else {
        return Err(graph_call_coverage());
    };
    let identity =
        CoreSymbolId::new(seed.as_str().to_owned()).map_err(|_| graph_call_coverage())?;
    map_graph_calls(
        reads,
        roots,
        &identity,
        report,
        &CallGraphSource {
            provider: slot.edge_provider(),
            trees: [slot.workspace_root(), reads.index().root()],
            source: text,
            unit: unit.clone(),
            direction: TraversalDirection::Outgoing,
        },
    )
}

async fn resolve_symbol_references(
    reads: &ReadService,
    engines: &EnginePool,
    identity: &CoreSymbolId,
    deadline: Instant,
) -> Result<SymbolReferences, RiftError> {
    let Some((slot, file, symbol)) = reference_source(reads, engines, identity)? else {
        return Ok(SymbolReferences::NotServed);
    };
    let language = file.syntax().language().clone();
    let target = ReferenceTarget::new(slot.workspace_root(), reads.index().root(), file, symbol)?;
    let sources = DocumentSources::default();
    let answered = Box::pin(references_on_engine(
        slot,
        &target,
        (false, &sources),
        deadline,
    ))
    .await;
    let (report, unconfirmed) = match answered {
        Ok((report, unconfirmed)) => (report, unconfirmed.then(|| slot.name().to_owned())),
        Err(error) if error.slug() == errors::lsp::engine_capability_absent::SLUG => {
            return Ok(SymbolReferences::NotServed);
        }
        Err(error) if error.slug() == errors::lsp::engine_analyzing::SLUG => {
            let attempts = error
                .context()
                .find(|(key, _)| *key == "attempts")
                .and_then(|(_, value)| value.parse::<u64>().ok())
                .unwrap_or(0);
            return Ok(SymbolReferences::Unsettled { language, attempts });
        }
        Err(error) => return error.fail(),
    };
    symbol_references(
        map_references(
            reads,
            identity,
            report,
            slot.workspace_root(),
            slot.edge_provider(),
        ),
        language,
        unconfirmed,
    )
}

/// What one mapped engine answer contributes to the read.
///
/// An answer the served bytes cannot carry drops the engine's contribution and
/// leaves the indexed edges standing. Every other failure is the engine
/// tier's own, and still refuses the read: a broken document URI or a workspace
/// root the server cannot address says nothing about the engine's revision, and
/// degrading it would hide a defect behind a warning.
fn symbol_references(
    mapped: Result<Vec<GraphHop>, RiftError>,
    language: Language,
    unconfirmed: Option<String>,
) -> Result<SymbolReferences, RiftError> {
    match mapped {
        Ok(edges) => Ok(SymbolReferences::Resolved {
            edges,
            language,
            unconfirmed,
        }),
        Err(error) if error.slug() == errors::server::read_engine_answer::SLUG => {
            Ok(SymbolReferences::Unmapped {
                language,
                detail: error.detail(),
            })
        }
        Err(error) => error.fail(),
    }
}

#[derive(Debug)]
struct ReferenceTarget {
    path: ProjectPath,
    language: String,
    source: String,
    utf8: Position,
    utf16: Position,
    uri: lsp_types::Uri,
    canonical_uri: lsp_types::Uri,
}

impl ReferenceTarget {
    /// The `begin` step of an exchange about this target: opens its file, with the index's
    /// bytes, on each live session the exchange runs on.
    fn opening(
        &self,
    ) -> impl for<'session> FnMut(
        &'session mut EngineSession,
    ) -> SessionFuture<'session, Result<(), RiftError>>
    + use<> {
        let path = self.path.clone();
        let language = self.language.clone();
        let source = self.source.clone();
        move |session: &mut EngineSession| {
            let path = path.clone();
            let language = language.clone();
            let source = source.clone();
            Box::pin(async move { session.open(&path, &language, source).await })
        }
    }

    /// The `finish` step of an exchange about this target: closes its file.
    ///
    /// A failed closing notification changes nothing the exchange already holds, and the
    /// failure stays recorded in the engine log.
    fn closing(
        &self,
    ) -> impl for<'session> FnMut(&'session mut EngineSession) -> SessionFuture<'session, ()> + use<>
    {
        let path = self.path.clone();
        move |session: &mut EngineSession| {
            let path = path.clone();
            Box::pin(async move {
                if let Err(error) = session.close(&path).await {
                    rift_tracing::warn!(
                        component = "engine",
                        operation = "textDocument/didClose",
                        %error,
                        "engine document close failed"
                    );
                }
            })
        }
    }

    fn new(
        workspace_root: &std::path::Path,
        canonical_root: &std::path::Path,
        file: &IndexedFile,
        symbol: &SyntaxSymbol,
    ) -> Result<Self, RiftError> {
        let mut target = Self::file(workspace_root, canonical_root, file)?;
        let offset = declaration_name_offset(file, symbol)?;
        let index = LineIndex::new(file.source());
        target.utf8 = index.position(PositionEncoding::Utf8, offset)?;
        target.utf16 = index.position(PositionEncoding::Utf16, offset)?;
        Ok(target)
    }

    fn file(
        workspace_root: &std::path::Path,
        canonical_root: &std::path::Path,
        file: &IndexedFile,
    ) -> Result<Self, RiftError> {
        Ok(Self {
            path: file.path().clone(),
            language: file.syntax().language().name.clone(),
            uri: TreeRoot::new(workspace_root)
                .and_then(|root| root.document_uri(file.path()))
                .map_err(|error| {
                    errors::server::read_task()
                        .operation("reference URI conversion")
                        .detail(error.detail())
                        .error()
                })?,
            canonical_uri: TreeRoot::new(canonical_root)
                .and_then(|root| root.document_uri(file.path()))
                .map_err(|error| {
                    errors::server::read_task()
                        .operation("reference URI conversion")
                        .detail(error.detail())
                        .error()
                })?,
            source: file.source().to_owned(),
            utf8: Position::default(),
            utf16: Position::default(),
        })
    }
}

type ReferenceDefinitions = BTreeMap<(String, u32, u32), Vec<Location>>;

/// The indexed files an engine exchange may open by path, each with the index's bytes.
///
/// A `textDocument/*` request names an open document: the base protocol guarantees answers
/// only for documents the client opened, and typescript-language-server answers `null` for
/// one it was not told about. An exchange that asks about a position in another file opens
/// that file from here first.
#[derive(Clone, Debug, Default)]
pub struct DocumentSources(std::sync::Arc<BTreeMap<ProjectPath, std::sync::Arc<IndexedFile>>>);

impl DocumentSources {
    /// The files `reads` indexes.
    #[must_use]
    pub fn of(reads: &ReadService) -> Self {
        Self(std::sync::Arc::new(
            reads
                .index()
                .shared_files()
                .map(|(path, file)| (path.clone(), std::sync::Arc::clone(file)))
                .collect(),
        ))
    }

    fn file(&self, path: &ProjectPath) -> Option<&IndexedFile> {
        self.0.get(path).map(AsRef::as_ref)
    }
}

/// Opens each document of `paths` the session does not hold open and `sources` holds, and
/// answers the paths it opened, which the caller closes after its requests.
///
/// Each opened document is charged against `remaining` as one record.
async fn open_documents(
    session: &mut EngineSession,
    sources: &DocumentSources,
    paths: BTreeSet<&ProjectPath>,
    remaining: &mut usize,
) -> Result<Vec<ProjectPath>, RiftError> {
    let mut opened = Vec::new();
    for path in paths {
        if session.is_open(path) {
            continue;
        }
        let Some(file) = sources.file(path) else {
            continue;
        };
        *remaining = remaining
            .checked_sub(1)
            .ok_or_else(graph_reference_coverage)?;
        session
            .open(
                path,
                &file.syntax().language().name,
                file.source().to_owned(),
            )
            .await?;
        opened.push(path.clone());
    }
    Ok(opened)
}

fn reference_key(location: &Location) -> (String, u32, u32) {
    (
        location.uri.as_str().to_owned(),
        location.range.start.line,
        location.range.start.character,
    )
}

async fn reference_definitions(
    session: &mut EngineSession,
    locations: &[Location],
    (maximum, mut remaining): (usize, usize),
    sources: &DocumentSources,
) -> Result<ReferenceDefinitions, RiftError> {
    let mut definitions = BTreeMap::new();
    if !session.capabilities().definitions.definition {
        return Ok(definitions);
    }
    let positions: BTreeMap<_, _> = locations
        .iter()
        .filter_map(|location| {
            let path = session.root().project_path(&location.uri).ok()?;
            Some((reference_key(location), (path, location.range.start)))
        })
        .collect();
    let positions: Vec<_> = positions.into_iter().collect();
    for chunk in positions.chunks(maximum.clamp(1, rift_lsp::correlation::PENDING_REQUESTS_MAX)) {
        let requests: Vec<_> = chunk.iter().map(|(_, position)| position.clone()).collect();
        let paths = requests.iter().map(|(path, _)| path).collect();
        let opened = open_documents(session, sources, paths, &mut remaining).await?;
        let answers = session.definition_batch(&requests).await;
        for path in &opened {
            session.close(path).await?;
        }
        let answers = answers?;
        let records = answers.iter().map(Vec::len).sum::<usize>();
        remaining = remaining
            .checked_sub(records)
            .ok_or_else(graph_reference_coverage)?;
        definitions.extend(
            chunk
                .iter()
                .zip(answers)
                .map(|((key, _), answer)| (key.clone(), answer)),
        );
    }
    Ok(definitions)
}

#[derive(PartialEq)]
struct ReferenceReport {
    definitions: std::sync::Arc<ReferenceDefinitions>,
    locations: Result<Vec<Location>, usize>,
    encoding: PositionEncoding,
    full: bool,
}

async fn references_on_engine(
    slot: &EngineSlot,
    target: &ReferenceTarget,
    (resolve_targets, sources): (bool, &DocumentSources),
    deadline: impl Into<EngineWalkDeadline>,
) -> Result<(ReferenceReport, bool), RiftError> {
    let request_path = target.path.clone();
    let sources = sources.clone();
    let utf8 = target.utf8;
    let utf16 = target.utf16;
    let answer = slot
        .request_outgoing(
            target.opening(),
            move |session: &mut EngineSession| {
                let path = request_path.clone();
                let sources = sources.clone();
                Box::pin(async move {
                    let encoding = session.capabilities().position_encoding;
                    let position = match encoding {
                        PositionEncoding::Utf8 => utf8,
                        PositionEncoding::Utf16 => utf16,
                    };
                    let mut locations = session.references(&path, position).await?;
                    if locations.len() > TRAVERSAL_SYMBOLS_MAX {
                        return Ok(Some(ReferenceReport {
                            locations: Err(locations.len()),
                            definitions: BTreeMap::new().into(),
                            encoding,
                            full: true,
                        }));
                    }
                    locations.sort_by(|left, right| {
                        (
                            left.uri.as_str(),
                            left.range.start.line,
                            left.range.start.character,
                            left.range.end.line,
                            left.range.end.character,
                        )
                            .cmp(&(
                                right.uri.as_str(),
                                right.range.start.line,
                                right.range.start.character,
                                right.range.end.line,
                                right.range.end.character,
                            ))
                    });
                    locations.dedup();
                    let definitions = if resolve_targets {
                        reference_definitions(
                            session,
                            &locations,
                            (
                                rift_lsp::correlation::PENDING_REQUESTS_MAX,
                                TRAVERSAL_SYMBOLS_MAX,
                            ),
                            &sources,
                        )
                        .await?
                    } else {
                        BTreeMap::new()
                    };
                    Ok(Some(ReferenceReport {
                        definitions: definitions.into(),
                        locations: Ok(locations),
                        encoding,
                        full: true,
                    }))
                })
            },
            target.closing(),
            deadline,
        )
        .await?;
    match answer {
        OutgoingAnswer::Ready(report) => Ok((report, false)),
        OutgoingAnswer::Unconfirmed(report) => Ok((report, true)),
        OutgoingAnswer::Unprepared => Err(graph_reference_coverage()),
        OutgoingAnswer::Unsettled { attempts } => {
            errors::lsp::engine_analyzing().attempts(attempts).fail()
        }
    }
}

/// Physical usages and provider coverage for one local declaration.
#[derive(Debug)]
pub struct GraphReferences {
    /// Captured physical usages.
    pub occurrences: Vec<QueryOccurrence>,
    /// Reference edges with their analysis and physical occurrences.
    pub edges: Vec<StoredEdge>,
    /// Whether a configured provider covered the request.
    pub complete: bool,
    /// Typed warnings from the engine edge read.
    pub edge_warnings: Vec<ReadWarning>,
}

/// Reads physical usages from captured source and configured engines.
///
/// # Errors
/// Refuses missing coverage or work outside the accepted bounds.
pub async fn graph_references(
    reads: &ReadService,
    engines: &EnginePool,
    target: &SymbolId,
    require_caller: bool,
    deadline: impl Into<EngineWalkDeadline>,
) -> Result<GraphReferences, RiftError> {
    let identity =
        CoreSymbolId::new(target.as_str().to_owned()).map_err(|_| graph_reference_coverage())?;
    let captured = captured_references(reads, &identity, target, require_caller)?;
    let language = rift_protocol::identity::SymbolIdentity::parse(target.as_str())?
        .language()
        .clone();
    if engines.engine_for(&language).is_none() {
        return Ok(GraphReferences {
            complete: captured.complete && reads.static_edges_complete()?,
            ..captured
        });
    }
    match Box::pin(engine_graph_references(
        reads,
        engines,
        target,
        require_caller,
        deadline,
    ))
    .await
    {
        Ok(engine) => Ok(GraphReferences {
            occurrences: merge_reference_occurrences(captured.occurrences, engine.occurrences)?,
            edges: captured.edges.into_iter().chain(engine.edges).collect(),
            complete: captured.complete && engine.complete,
            edge_warnings: engine.edge_warnings,
        }),
        Err(error)
            if error.slug() == errors::server::search_coverage_unavailable::SLUG
                && !captured.occurrences.is_empty() =>
        {
            Ok(GraphReferences {
                complete: false,
                ..captured
            })
        }
        Err(error) if error.slug() == errors::lsp::engine_analyzing::SLUG => {
            let language = rift_protocol::identity::SymbolIdentity::parse(target.as_str())?
                .language()
                .clone();
            let attempts = error
                .context()
                .find(|(key, _)| *key == "attempts")
                .and_then(|(_, value)| value.parse().ok())
                .unwrap_or_default();
            Ok(GraphReferences {
                complete: false,
                edge_warnings: vec![walk_unsettled_warning(language, attempts)],
                ..captured
            })
        }
        Err(error) => Err(error),
    }
}

/// Reads the physical usages the captured static pass resolved for one local declaration,
/// without a language engine.
///
/// # Errors
/// Refuses a target that is not a canonical identity or a captured path that is invalid.
pub fn captured_graph_references(
    reads: &ReadService,
    target: &SymbolId,
    require_caller: bool,
) -> Result<GraphReferences, RiftError> {
    let identity =
        CoreSymbolId::new(target.as_str().to_owned()).map_err(|_| graph_reference_coverage())?;
    captured_references(reads, &identity, target, require_caller)
}

fn captured_references(
    reads: &ReadService,
    identity: &CoreSymbolId,
    target: &SymbolId,
    require_caller: bool,
) -> Result<GraphReferences, RiftError> {
    let mut occurrences = Vec::new();
    let mut complete = true;
    for (position, (binding, range, digest, _)) in reads.index().static_usages(identity).enumerate()
    {
        if position == TRAVERSAL_SYMBOLS_MAX {
            complete = false;
            break;
        }
        let path = ProjectPath::new(binding.unit().key().as_str())?;
        let Some(file) = reads
            .index()
            .file(&path)
            .filter(|file| file.digest() == digest)
        else {
            complete = false;
            continue;
        };
        let captured_edge = reads.edges().incoming(identity).iter().find(|edge| {
            edge.occurrence().unit() == binding.unit()
                && edge.occurrence().range().start() <= range.start
                && range.end <= edge.occurrence().range().end()
        });
        let enclosing = match captured_edge {
            Some(edge) => Some(SymbolId(edge.from().as_str().to_owned())),
            None => edge_enclosing_symbol(reads, file, range.start, range.end)?,
        };
        if require_caller && enclosing.is_none() {
            continue;
        }
        occurrences.push(QueryOccurrence {
            target: target.clone(),
            enclosing,
            source: SourceUnitSpan {
                unit: crate::read::source_unit_id(&path),
                range: TextRange {
                    start: range.start,
                    end: range.end,
                },
            },
            kind: EdgeKind::References,
        });
    }
    Ok(GraphReferences {
        edges: reference_edges(&occurrences, EdgeProvider::Static),
        occurrences,
        complete,
        edge_warnings: Vec::new(),
    })
}

fn merge_reference_occurrences(
    captured: Vec<QueryOccurrence>,
    engine: Vec<QueryOccurrence>,
) -> Result<Vec<QueryOccurrence>, RiftError> {
    let mut occurrences = BTreeMap::new();
    for occurrence in engine.into_iter().chain(captured) {
        let key = (
            &occurrence.source.unit.0,
            occurrence.source.range.start,
            occurrence.source.range.end,
        );
        occurrences.insert((key.0.clone(), key.1, key.2), occurrence);
        if occurrences.len() > TRAVERSAL_SYMBOLS_MAX {
            return Err(graph_reference_coverage());
        }
    }
    Ok(occurrences.into_values().collect())
}

/// Complete physical usages from configured engines for one local declaration.
///
/// # Errors
/// Refuses missing readiness, an uncovered source location, or a response above the engine bound.
async fn engine_graph_references(
    reads: &ReadService,
    engines: &EnginePool,
    target: &SymbolId,
    require_caller: bool,
    deadline: impl Into<EngineWalkDeadline>,
) -> Result<GraphReferences, RiftError> {
    let unavailable = graph_reference_coverage;
    let identity = CoreSymbolId::new(target.as_str().to_owned()).map_err(|_| unavailable())?;
    let declarations = edge_declarations(reads, &identity)?;
    if declarations.is_empty() {
        return Err(unavailable());
    }
    let mut complete = true;
    let mut found = BTreeMap::new();
    let mut edges = Vec::new();
    let mut edge_warnings = Vec::new();
    let sources = DocumentSources::of(reads);
    let deadline = deadline.into();
    for declaration in declarations {
        let file = declaration.file;
        let slot = engines
            .engine_for(file.syntax().language())
            .ok_or_else(unavailable)?;
        let request = ReferenceTarget::new(
            slot.workspace_root(),
            reads.index().root(),
            file,
            declaration.symbol,
        )?;
        let (report, unconfirmed) = Box::pin(references_on_engine(
            slot,
            &request,
            (true, &sources),
            deadline,
        ))
        .await
        .map_err(|error| {
            if graph_engine_method_missing(&error) {
                unavailable()
            } else {
                error
            }
        })?;
        if !report.full {
            return Err(unavailable());
        }
        if unconfirmed {
            let processes = BTreeSet::from([slot.name().to_owned()]);
            edge_warnings.push(readiness_unconfirmed_warning(&processes));
        }
        let mapped = map_inventory_references(
            reads,
            target,
            report,
            slot.workspace_root(),
            require_caller,
            slot.edge_provider(),
        )?;
        edges.extend(mapped.edges);
        complete &= mapped.complete;
        edge_warnings.extend(mapped.edge_warnings);
        for occurrence in mapped.occurrences {
            let key = (
                occurrence.source.unit.0.clone(),
                occurrence.source.range.start,
                occurrence.source.range.end,
            );
            found.insert(key, occurrence);
        }
    }
    Ok(GraphReferences {
        occurrences: found.into_values().collect(),
        edges,
        complete,
        edge_warnings,
    })
}

/// Maps the local part of an engine reference answer into the captured index.
fn map_inventory_references(
    reads: &ReadService,
    target: &SymbolId,
    report: ReferenceReport,
    workspace_root: &std::path::Path,
    require_caller: bool,
    provider: EdgeProvider,
) -> Result<GraphReferences, RiftError> {
    let mut mapping = ReferenceMapping::new(reads, workspace_root, report.encoding, provider)?;
    map_inventory_references_with_mapping(target, report, require_caller, &mut mapping)
}

fn map_inventory_references_with_mapping(
    target: &SymbolId,
    report: ReferenceReport,
    require_caller: bool,
    mapping: &mut ReferenceMapping<'_>,
) -> Result<GraphReferences, RiftError> {
    let reads = mapping.reads;
    if report.encoding != mapping.encoding {
        return Err(graph_reference_coverage());
    }
    let identity = CoreSymbolId::new(target.as_str())?;
    let excluded: BTreeSet<_> = edge_declarations(reads, &identity)?
        .into_iter()
        .filter_map(|declaration| {
            declaration
                .symbol
                .name_range
                .map(|range| (declaration.file.path().clone(), range.start, range.end))
        })
        .collect();
    let mut dropped = 0_u64;
    let mut found = BTreeMap::new();
    for location in report.locations.map_err(|_| graph_reference_coverage())? {
        let path = mapping.roots[0]
            .project_path(&location.uri)
            .or_else(|_| mapping.roots[1].project_path(&location.uri));
        let path = match path {
            Ok(path) => path,
            Err(error) if error.slug() == errors::lsp::uri_outside_root::SLUG => continue,
            Err(error) => return Err(error),
        };
        let Some(file) = reads.index().file(&path) else {
            continue;
        };
        let (start, end) = mapping.range(file, location.range)?;
        if file.source().get(start..end).is_none() {
            return Err(graph_reference_coverage());
        }
        let key = (path, start as u64, end as u64);
        // An engine can name a quoted property key without its quotes: a location inside
        // the declaration's own name is that declaration, not a reference to it.
        let declared = excluded
            .iter()
            .any(|(path, from, to)| *path == key.0 && *from <= key.1 && key.2 <= *to);
        if declared || found.contains_key(&key) {
            continue;
        }
        let resolved = mapping.targets(&report.definitions, &location)?;
        if resolved.is_empty() {
            dropped += 1;
        }
        if !resolved.contains(target) {
            continue;
        }
        if found.len() == TRAVERSAL_SYMBOLS_MAX {
            return Err(graph_reference_coverage());
        }
        let enclosing = edge_enclosing_symbol(reads, file, key.1, key.2)?;
        if require_caller && enclosing.is_none() {
            continue;
        }
        found.insert(
            key.clone(),
            QueryOccurrence {
                target: target.clone(),
                enclosing,
                source: SourceUnitSpan {
                    unit: crate::read::source_unit_id(file.path()),
                    range: TextRange {
                        start: key.1,
                        end: key.2,
                    },
                },
                kind: EdgeKind::References,
            },
        );
    }
    let occurrences: Vec<_> = found.into_values().collect();
    Ok(GraphReferences {
        edges: reference_edges(&occurrences, mapping.provider),
        occurrences,
        complete: report.full && dropped == 0,
        edge_warnings: if dropped == 0 {
            Vec::new()
        } else {
            vec![ReadWarning::EngineAnalysisUnavailable {
                language: None,
                detail: format!(
                    "the language engine did not resolve a local definition for {dropped} reference locations; those locations were dropped"
                ),
            }]
        },
    })
}

type DefinitionLocationKey = (String, u32, u32, u32, u32);

/// Reuses only positions and definitions from one captured inventory.
struct ReferenceMapping<'a> {
    reads: &'a ReadService,
    roots: [TreeRoot; 2],
    encoding: PositionEncoding,
    provider: EdgeProvider,
    positions: BTreeMap<ProjectPath, LineIndex<'a>>,
    definitions: BTreeMap<DefinitionLocationKey, Option<SymbolId>>,
    #[cfg(test)]
    line_index_builds: usize,
    #[cfg(test)]
    definition_resolutions: usize,
}

impl<'a> ReferenceMapping<'a> {
    fn new(
        reads: &'a ReadService,
        workspace_root: &std::path::Path,
        encoding: PositionEncoding,
        provider: EdgeProvider,
    ) -> Result<Self, RiftError> {
        Ok(Self {
            reads,
            roots: [
                TreeRoot::new(workspace_root)?,
                TreeRoot::new(reads.index().root())?,
            ],
            encoding,
            provider,
            positions: BTreeMap::new(),
            definitions: BTreeMap::new(),
            #[cfg(test)]
            line_index_builds: 0,
            #[cfg(test)]
            definition_resolutions: 0,
        })
    }

    fn range(&mut self, file: &'a IndexedFile, range: Range) -> Result<(usize, usize), RiftError> {
        let positions = match self.positions.entry(file.path().clone()) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                #[cfg(test)]
                {
                    self.line_index_builds += 1;
                }
                entry.insert(LineIndex::new(file.source()))
            }
        };
        Ok((
            positions.byte_offset(self.encoding, range.start)?,
            positions.byte_offset(self.encoding, range.end)?,
        ))
    }

    fn targets(
        &mut self,
        definitions: &ReferenceDefinitions,
        location: &Location,
    ) -> Result<BTreeSet<SymbolId>, RiftError> {
        let mut resolved = BTreeSet::new();
        for definition in definitions
            .get(&reference_key(location))
            .into_iter()
            .flatten()
        {
            if let Some(symbol) = self.definition(definition)? {
                resolved.insert(symbol);
            }
        }
        Ok(resolved)
    }

    fn definition(&mut self, location: &Location) -> Result<Option<SymbolId>, RiftError> {
        let key = (
            location.uri.as_str().to_owned(),
            location.range.start.line,
            location.range.start.character,
            location.range.end.line,
            location.range.end.character,
        );
        if let Some(symbol) = self.definitions.get(&key) {
            return Ok(symbol.clone());
        }
        let path = self.roots[0]
            .project_path(&location.uri)
            .or_else(|_| self.roots[1].project_path(&location.uri));
        let mut symbol = None;
        if let Ok(path) = path
            && let Some(file) = self.reads.index().file(&path)
        {
            let (start, end) = self.range(file, location.range)?;
            symbol = project_definition_at(self.reads, file, (start as u64, end as u64), false)?;
        }
        #[cfg(test)]
        {
            self.definition_resolutions += 1;
        }
        self.definitions.insert(key, symbol.clone());
        Ok(symbol)
    }
}

/// Reads outgoing reference edges by resolving the bounded local target inventory.
///
/// # Errors
/// Refuses incomplete sources, an inventory above the traversal bound, or engine failure.
///
/// # Cancel safety
/// Dropping the future cancels the current engine exchange and publishes no retained answer.
pub async fn outgoing_reference_edges(
    reads: &ReadService,
    engines: &EnginePool,
    seed: &SymbolId,
    deadline: Instant,
) -> Result<GraphReferences, RiftError> {
    let identity = CoreSymbolId::new(seed.as_str())?;
    let adjacent = reads.edges().outgoing(&identity);
    if adjacent.len() > TRAVERSAL_SYMBOLS_MAX {
        return Err(graph_reference_coverage());
    }
    let captured = adjacent
        .iter()
        .filter(|edge| {
            matches!(
                edge.kind(),
                EdgeKind::Calls
                    | EdgeKind::References
                    | EdgeKind::Imports
                    | EdgeKind::Aliases
                    | EdgeKind::Reads
                    | EdgeKind::Writes
                    | EdgeKind::HasType
            )
        })
        .map(|edge| QueryOccurrence {
            target: SymbolId(edge.to().as_str().to_owned()),
            enclosing: Some(seed.clone()),
            source: SourceUnitSpan {
                unit: SourceUnitId(edge.occurrence().unit().to_string()),
                range: TextRange {
                    start: edge.occurrence().range().start(),
                    end: edge.occurrence().range().end(),
                },
            },
            kind: EdgeKind::References,
        })
        .collect::<Vec<_>>();
    let mut answer = GraphReferences {
        edges: reference_edges(&captured, EdgeProvider::Static),
        occurrences: captured,
        complete: true,
        edge_warnings: Vec::new(),
    };
    let language = rift_protocol::identity::SymbolIdentity::parse(seed.as_str())?
        .language()
        .clone();
    if engines.engine_for(&language).is_none() {
        answer.complete = reads.edges().is_complete();
        return Ok(answer);
    }
    let seeds = edge_reference_targets(reads, &language)?;
    let mut admitted: BTreeSet<_> = answer.edges.iter().map(reference_edge_key).collect();
    for target in seeds {
        let incoming = Box::pin(graph_references(reads, engines, &target, true, deadline)).await?;
        answer.complete &= incoming.complete;
        for warning in incoming.edge_warnings {
            if !answer.edge_warnings.contains(&warning) {
                answer.edge_warnings.push(warning);
            }
        }
        for edge in incoming.edges {
            if edge.from == *seed && admitted.insert(reference_edge_key(&edge)) {
                answer.edges.push(edge);
            }
        }
        let outgoing = incoming
            .occurrences
            .into_iter()
            .filter(|occurrence| occurrence.enclosing.as_ref() == Some(seed))
            .collect();
        answer.occurrences = merge_reference_occurrences(answer.occurrences, outgoing)?;
        if !incoming.complete {
            break;
        }
    }
    Ok(answer)
}

fn edge_declarations<'read>(
    reads: &'read ReadService,
    identity: &CoreSymbolId,
) -> Result<Vec<rift_index::SymbolMatch<'read>>, RiftError> {
    let mut declarations = all_edge_declarations(reads, identity)?;
    declarations.retain(|declaration| declaration.symbol.name_range.is_some());
    Ok(declarations)
}

fn all_edge_declarations<'read>(
    reads: &'read ReadService,
    identity: &CoreSymbolId,
) -> Result<Vec<rift_index::SymbolMatch<'read>>, RiftError> {
    let limit = reads.index().results_max().min(TRAVERSAL_SYMBOLS_MAX);
    let (mut declarations, total) = reads.index().symbols_by_identity_page(identity, 0, limit)?;
    if total > TRAVERSAL_SYMBOLS_MAX || declarations.len() != total.min(limit) {
        return Err(graph_reference_coverage());
    }
    while declarations.len() < total {
        let remaining = total - declarations.len();
        let (page, _) = reads.index().symbols_by_identity_page(
            identity,
            declarations.len(),
            remaining.min(limit),
        )?;
        if page.len() != remaining.min(limit) {
            return Err(graph_reference_coverage());
        }
        declarations.extend(page);
    }
    Ok(declarations)
}

fn edge_reference_targets(
    reads: &ReadService,
    language: &Language,
) -> Result<BTreeSet<SymbolId>, RiftError> {
    if !reads.index().symbol_view()?.objects_complete() {
        return Err(graph_reference_coverage());
    }
    let mut targets = BTreeSet::new();
    for file in reads
        .index()
        .files()
        .filter(|file| file.syntax().language().name == language.name)
    {
        for declaration in file
            .syntax()
            .symbols()
            .iter()
            .filter(|declaration| declaration.name_range.is_some())
        {
            let Some(target) =
                symbol_id(reads.index(), crate::search::declared(file, declaration))?
            else {
                continue;
            };
            targets.insert(target);
            if targets.len() > TRAVERSAL_SYMBOLS_MAX {
                return Err(graph_reference_coverage());
            }
        }
    }
    Ok(targets)
}

fn edge_enclosing_symbol(
    reads: &ReadService,
    file: &IndexedFile,
    start: u64,
    end: u64,
) -> Result<Option<SymbolId>, RiftError> {
    if let Some(symbol) = file.enclosing_symbol(start, end) {
        return symbol_id(reads.index(), crate::search::declared(file, symbol));
    }
    let placement = rift_syntax::DocumentPlacement::project_path(file.path())?;
    Ok(reads
        .edges()
        .edge_module(placement.unit())
        .map(|id| SymbolId(id.as_str().to_owned())))
}

fn reference_edges(occurrences: &[QueryOccurrence], provider: EdgeProvider) -> Vec<StoredEdge> {
    occurrences
        .iter()
        .filter_map(|occurrence| {
            Some(graph_edge(
                occurrence.enclosing.as_ref()?,
                occurrence.target.clone(),
                EdgeKind::References,
                Some(occurrence.source.clone()),
                provider,
            ))
        })
        .collect()
}

/// One reference edge's target, physical occurrence, and analysis.
fn reference_edge_key(edge: &StoredEdge) -> (SymbolId, Option<(String, u64, u64)>, EdgeProvider) {
    (
        edge.to.clone(),
        edge.occurrence.as_ref().map(|span| {
            (
                span.unit.as_str().to_owned(),
                span.range.start,
                span.range.end,
            )
        }),
        edge.provider,
    )
}

fn graph_reference_coverage() -> RiftError {
    errors::server::search_coverage_unavailable()
        .operation("graph references")
        .error()
}

fn map_references(
    reads: &ReadService,
    target: &CoreSymbolId,
    report: ReferenceReport,
    workspace_root: &std::path::Path,
    provider: EdgeProvider,
) -> Result<Vec<GraphHop>, RiftError> {
    let locations = report.locations.map_err(|observed| errors::server::read_engine_answer().operation("engine references").detail(format!("reference locations {observed} exceed the {TRAVERSAL_SYMBOLS_MAX}-symbol traversal bound")).error())?;
    let root = TreeRoot::new(workspace_root).map_err(|error| {
        errors::server::read_task()
            .operation("reference root conversion")
            .detail(error.detail())
            .error()
    })?;
    let mut callers = BTreeSet::new();
    for location in locations {
        let at = EngineLocation {
            uri: &location.uri,
            range: location.range,
            encoding: report.encoding,
        };
        if let Some(caller) = reference_caller(reads, &root, at)?
            && caller.0 != target.as_str()
        {
            callers.insert(caller);
        }
    }
    Ok(callers
        .into_iter()
        .map(|caller| GraphHop {
            edge: graph_edge(
                &caller,
                SymbolId(target.as_str().to_owned()),
                EdgeKind::References,
                None,
                provider,
            )
            .into(),
            direction: HopDirection::Incoming,
        })
        .collect())
}

/// One range an engine named in a file, with the position encoding it counts in.
#[derive(Clone, Copy)]
struct EngineLocation<'location> {
    uri: &'location Uri,
    range: Range,
    encoding: PositionEncoding,
}

/// The declaration whose complete span, attached documentation included, holds one
/// reference location: the caller an incoming hop names. `None` for a location outside the
/// served tree or outside every declaration of its file.
fn reference_caller(
    reads: &ReadService,
    root: &TreeRoot,
    at: EngineLocation<'_>,
) -> Result<Option<SymbolId>, RiftError> {
    let relative = root.project_path(at.uri).or_else(|error| {
        if error.slug() == errors::lsp::uri_outside_root::SLUG {
            TreeRoot::new(reads.index().root())?.project_path(at.uri)
        } else {
            error.fail()
        }
    });
    let path = match relative {
        Ok(path) => path,
        Err(error) if error.slug() == errors::lsp::uri_outside_root::SLUG => return Ok(None),
        Err(error) => return error.fail(),
    };
    project_declaration(reads, &path, (at.range, at.encoding), "engine references")
}

/// The declaration of the project file at `path` whose complete span, attached
/// documentation included, holds `range`: the caller holding a reference, or the callee
/// whose name an outgoing call names. `None` for a file this tree does not index, or a
/// range outside every declaration of it.
fn project_declaration(
    reads: &ReadService,
    path: &ProjectPath,
    (range, encoding): (Range, PositionEncoding),
    operation: &'static str,
) -> Result<Option<SymbolId>, RiftError> {
    let Some(file) = reads.index().file(path) else {
        return Ok(None);
    };
    let index = LineIndex::new(file.source());
    let convert = |position| {
        index.byte_offset(encoding, position).map_err(|error| {
            errors::server::read_engine_answer()
                .operation(operation)
                .detail(error.detail())
                .error()
        })
    };
    let start = convert(range.start)? as u64;
    let end = convert(range.end)? as u64;
    if start > end {
        return errors::server::read_engine_answer()
            .operation(operation)
            .detail("an engine range ends before it starts")
            .fail();
    }
    let Some(symbol) = file.enclosing_symbol(start, end) else {
        return Ok(None);
    };
    symbol_id(reads.index(), crate::search::declared(file, symbol))
}

/// Names an engine definition only when its range belongs to a declaration's name.
fn project_definition(
    reads: &ReadService,
    path: &ProjectPath,
    (range, encoding): (Range, PositionEncoding),
    require_type: bool,
) -> Result<Option<SymbolId>, RiftError> {
    let Some(file) = reads.index().file(path) else {
        return Ok(None);
    };
    let lines = LineIndex::new(file.source());
    let start = lines.byte_offset(encoding, range.start)? as u64;
    let end = lines.byte_offset(encoding, range.end)? as u64;
    project_definition_at(reads, file, (start, end), require_type)
}

fn project_definition_at(
    reads: &ReadService,
    file: &IndexedFile,
    (start, end): (u64, u64),
    require_type: bool,
) -> Result<Option<SymbolId>, RiftError> {
    let Some(symbol) = file.enclosing_symbol(start, end) else {
        return Ok(None);
    };
    let Some(name) = symbol.name_range else {
        return Ok(None);
    };
    let named =
        (name.start <= start && end <= name.end) || (start <= name.start && name.end <= end);
    if !named
        || symbol
            .kinds
            .contains(&rift_protocol::read::SymbolKind::Module)
        || (require_type
            && !symbol
                .kinds
                .contains(&rift_protocol::read::SymbolKind::Type))
    {
        return Ok(None);
    }
    symbol_id(reads.index(), crate::search::declared(file, symbol))
}

/// Reads the exact name range retained by the syntax provider.
fn declaration_name_offset(file: &IndexedFile, symbol: &SyntaxSymbol) -> Result<usize, RiftError> {
    let range = symbol.name_range.ok_or_else(|| {
        errors::server::read_unsupported()
            .capability("declaration name range")
            .error()
    })?;
    let invalid = || {
        errors::server::read_task()
            .operation("reference position conversion")
            .detail("declaration name range exceeds indexed source")
            .error()
    };
    let start = usize::try_from(range.start).map_err(|_| invalid())?;
    let end = usize::try_from(range.end).map_err(|_| invalid())?;
    if file.source().get(start..end).is_none() {
        return invalid().fail();
    }
    Ok(start)
}

#[cfg(all(test, unix))]
#[path = "../tests/process_lifecycle.rs"]
#[expect(dead_code, reason = "Each suite selects only its process fixtures.")]
mod process_lifecycle;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use lsp_types::{Location, Position, Range};
    use rift_core::{ProjectPath, SourceVisibility, TextFileInclusion};
    use rift_error::errors;
    use rift_index::WorkspaceIndexLimits;
    use rift_lsp::capabilities::PositionEncoding;
    use rift_lsp::uri::TreeRoot;
    use rift_protocol::configuration::{HistoryConfiguration, LspConfiguration};
    use rift_protocol::read::{EdgeProvider, ExactKind, GetSymbolParams, SearchParams, SymbolId};
    use serde_json::json;

    use super::{
        CalleeReport, EngineReferences, ReferenceReport, declaration_name_offset, map_callees,
        map_references, resolve_engine_references,
    };
    use crate::callee::{CalleeDeclaration, CalleeRoots};
    use crate::search::StoreAnswer;
    use crate::{EnginePool, LspProcessKey, ReadService};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// A walk deadline no test reaches.
    fn far() -> tokio::time::Instant {
        tokio::time::Instant::now() + std::time::Duration::from_secs(60)
    }

    /// A walk's far deadline, with no installed package to address a callee through.
    fn walk() -> (tokio::time::Instant, &'static CalleeRoots) {
        static NO_PACKAGES: std::sync::OnceLock<CalleeRoots> = std::sync::OnceLock::new();
        (far(), NO_PACKAGES.get_or_init(CalleeRoots::default))
    }

    #[test]
    fn callee_hops_keep_one_edge_per_callee_id() {
        let id = |name: &str| SymbolId(format!("rift://symbol/python/lib.py/{name}"));
        let hops = super::callee_hops(
            &id("seed"),
            ["max", "maximum", "max", "join", "seed", "maximum"].map(id),
            EdgeProvider::Engine,
            BTreeMap::new(),
        );
        let reached: Vec<_> = hops
            .iter()
            .map(|hop| hop.edge.to.0.rsplit('/').next().unwrap_or_default())
            .collect();
        assert_eq!(reached, ["join", "max", "maximum"]);
        for hop in &hops {
            assert_eq!(hop.direction, rift_protocol::read::HopDirection::Outgoing);
            assert_eq!(hop.edge.from, id("seed"));
            assert_eq!(hop.edge.kinds, [rift_protocol::read::EdgeKind::Calls]);
        }
    }

    #[test]
    fn outgoing_walk_attributes_only_the_exact_selected_engine_call() -> TestResult {
        use rift_lsp::position::LineIndex;
        use rift_protocol::read::{EdgeKind, SearchTraversal, TraversalDirection};

        let directory = tempfile::tempdir()?;
        let source =
            "pub fn beacon() {}\npub fn caller() { beacon(); beacon(); }\npub fn decoy() {}\n";
        fs::write(directory.path().join("lib.rs"), source)?;
        let reads = reads(directory.path())?;
        let caller = symbol(&reads, "caller");
        let target = symbol(&reads, "beacon");
        let caller_id = rift_core::SymbolId::new(caller.0.clone())?;
        let target_id = rift_core::SymbolId::new(target.0.clone())?;
        let file = reads
            .index()
            .file(&ProjectPath::new("lib.rs")?)
            .ok_or("caller file")?;
        let store = reads.edges();
        let selected: Vec<_> = store
            .outgoing(&caller_id)
            .iter()
            .filter(|edge| edge.to() == &target_id && edge.kind() == EdgeKind::Calls)
            .collect();
        let [first, second] = selected.as_slice() else {
            return Err("two distinct authored calls expected".into());
        };
        let line = LineIndex::new(source);
        let mut exact = item(location(directory.path(), "lib.rs", 0, 7, 13), "beacon");
        exact.from_ranges.push(utf16_call_range(&line, first)?);
        let mut other = exact.clone();
        other.from_ranges = vec![utf16_call_range(&line, second)?];
        let mut neighbor = exact.clone();
        neighbor.from_ranges.clear();
        let mut ambiguous = item(location(directory.path(), "lib.rs", 2, 7, 12), "decoy");
        ambiguous.from_ranges.clone_from(&exact.from_ranges);
        let mut overflow = exact.clone();
        overflow.from_ranges.resize(
            super::TRAVERSAL_SYMBOLS_MAX + 1,
            utf16_call_range(&line, first)?,
        );
        let traversal: SearchTraversal = serde_json::from_value(json!({
            "seed": caller, "direction": TraversalDirection::Outgoing, "kinds": ["calls"], "depth": 1
        }))?;
        let roots = CalleeRoots::default();
        for (label, calls, lsp) in [
            ("same occurrence", vec![exact.clone()], true),
            ("different occurrence", vec![other], false),
            ("neighbor only", vec![neighbor], false),
            ("multiple destinations", vec![exact, ambiguous], false),
            (
                "all locations exceed the corroboration budget",
                vec![overflow],
                false,
            ),
            ("ready empty", Vec::new(), false),
        ] {
            let mapped = map_callees(
                &reads,
                CalleeReport {
                    calls: Ok(calls),
                    encoding: PositionEncoding::Utf16,
                },
                &caller_id,
                (&roots, &[directory.path()]),
                EdgeProvider::Lsp,
                Some(file),
            )?;
            let hops = super::callee_hops(
                &caller,
                mapped.callees,
                EdgeProvider::Lsp,
                mapped.corroborated,
            );
            let engine =
                EngineReferences::from_outgoing(BTreeMap::from([(caller_id.clone(), hops)]));
            let walk = crate::traversal::walk_traversal_with_references(
                store,
                &caller_id,
                &traversal,
                super::TRAVERSAL_SYMBOLS_MAX,
                &engine,
            );
            assert_selected_call_providers(&walk, first, label, lsp);
        }
        let walk = crate::traversal::walk_traversal_with_references(
            store,
            &caller_id,
            &traversal,
            super::TRAVERSAL_SYMBOLS_MAX,
            &EngineReferences::default(),
        );
        assert_selected_call_providers(&walk, first, "no engine", false);
        Ok(())
    }

    fn utf16_call_range(
        line: &rift_lsp::position::LineIndex<'_>,
        edge: &rift_index::Edge,
    ) -> Result<Range, Box<dyn std::error::Error>> {
        let bytes = edge.occurrence().range();
        Ok(Range::new(
            line.position(PositionEncoding::Utf16, usize::try_from(bytes.start())?)?,
            line.position(PositionEncoding::Utf16, usize::try_from(bytes.end())?)?,
        ))
    }

    fn assert_selected_call_providers(
        walk: &rift_index::TraversalWalk,
        stored: &rift_index::Edge,
        label: &str,
        lsp: bool,
    ) {
        let mut paths = walk.discovered.iter().filter(|(id, _)| id == stored.to());
        let Some((_, path)) = paths.next() else {
            panic!("{label}: target expected")
        };
        assert!(paths.next().is_none(), "{label}: one target expected");
        let [hop] = path.as_slice() else {
            panic!("{label}: one hop expected")
        };
        assert_eq!(
            hop.edge.occurrence,
            stored.to_protocol().occurrence,
            "{label}"
        );
        let mut expected = std::collections::BTreeSet::from([EdgeProvider::Static]);
        if lsp {
            expected.insert(EdgeProvider::Lsp);
        }
        assert_eq!(hop.edge.providers, expected, "{label}");
    }

    #[test]
    fn reference_targets_keep_identities_beside_a_variance_parse_error() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("package.json"),
            r#"{"name":"beacon"}"#,
        )?;
        fs::write(
            directory.path().join("index.ts"),
            // A declaration whose own value is unreadable keeps no identity; a variance
            // annotation no longer serves, since its declaration keeps one (D26).
            "export const Broken = { value: (globalThis as any)?.value ?? }\n\
             export function target() {}\nexport function caller() { target(); }\n",
        )?;
        let reads = ReadService::build(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )?;
        let language = rift_protocol::read::Language::from_identity_segment("typescript")?;
        assert!(!reads.index().identity_coverage_complete(&language)?);
        let targets = super::edge_reference_targets(&reads, &language)?;
        assert!(targets.contains(&symbol(&reads, "caller")));
        assert!(targets.contains(&symbol(&reads, "target")));
        assert!(reads.static_edges_complete()?);
        assert!(reads.edge_identity_warning().is_some());
        Ok(())
    }

    #[test]
    fn a_spent_walk_wait_warns_with_the_existing_engine_analysis_code() {
        let warning = super::walk_unsettled_warning(
            rift_protocol::read::Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            18,
        );
        let wire = serde_json::to_value(&warning).expect("warning serializes");
        assert_eq!(wire["code"], "engine_analysis_unavailable");
        assert!(
            wire["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("readiness_timeout")),
            "{wire}"
        );
    }

    /// The detail counts one attempt in the singular and any other count in the plural.
    #[test]
    fn a_spent_walk_wait_counts_its_attempts_in_the_detail() {
        let detail = |attempts| {
            let warning = super::walk_unsettled_warning(
                rift_protocol::read::Language {
                    name: "rust".to_owned(),
                    dialect: None,
                },
                attempts,
            );
            serde_json::to_value(&warning).expect("warning serializes")["detail"]
                .as_str()
                .expect("the warning carries a detail")
                .to_owned()
        };
        for (attempts, counted) in [
            (1, "after 1 attempt when"),
            (2, "after 2 attempts when"),
            (18, "after 18 attempts when"),
        ] {
            let detail = detail(attempts);
            assert!(detail.contains(counted), "{detail}");
        }
    }

    /// One call hierarchy item naming `name` at `location`.
    fn item(location: Location, name: &str) -> lsp_types::CallHierarchyOutgoingCall {
        lsp_types::CallHierarchyOutgoingCall {
            to: lsp_types::CallHierarchyItem {
                name: name.to_owned(),
                kind: lsp_types::SymbolKind::FUNCTION,
                tags: None,
                detail: None,
                uri: location.uri,
                range: location.range,
                selection_range: location.range,
                data: None,
            },
            from_ranges: Vec::new(),
        }
    }

    #[test]
    fn graph_calls_keep_mapped_occurrences_when_an_endpoint_is_unaddressed() -> TestResult {
        let directory = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let source = "pub fn beacon() {}\npub fn larger() { beacon(); }\n";
        fs::write(directory.path().join("lib.rs"), source)?;
        let reads = reads(directory.path())?;
        let mut valid = item(location(directory.path(), "lib.rs", 0, 7, 13), "beacon");
        valid
            .from_ranges
            .push(Range::new(Position::new(1, 18), Position::new(1, 24)));
        let absent = item(location(outside.path(), "lib.rs", 0, 7, 13), "outside");
        let report = CalleeReport {
            calls: Ok(vec![absent, valid]),
            encoding: PositionEncoding::Utf16,
        };
        let caller = rift_core::SymbolId::new(symbol(&reads, "larger").0)?;
        let roots = CalleeRoots::default();
        let held = super::CallGraphSource {
            provider: EdgeProvider::Lsp,
            trees: [directory.path(), directory.path()],
            source,
            unit: crate::read::source_unit_id(&ProjectPath::new("lib.rs")?),
            direction: rift_protocol::read::TraversalDirection::Outgoing,
        };
        let graph = super::map_graph_calls(&reads, &roots, &caller, report, &held)?;
        assert!(!graph.complete);
        let [(target, sites)] = graph.local.as_slice() else {
            panic!("expected the mapped callee and its call site");
        };
        assert_eq!(target, &symbol(&reads, "beacon"));
        let [site] = sites.as_slice() else {
            panic!("expected one original call site");
        };
        assert_eq!(site.unit, held.unit);
        assert_eq!(
            source.get(usize::try_from(site.range.start)?..usize::try_from(site.range.end)?),
            Some("beacon")
        );
        Ok(())
    }

    #[test]
    fn graph_calls_keep_only_sites_owned_by_the_selected_callable() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source = "pub fn leaf() {}\npub fn root() { let nested = || { leaf(); }; leaf(); }\n";
        fs::write(directory.path().join("lib.rs"), source)?;
        let reads = reads(directory.path())?;
        let line = source.lines().nth(1).ok_or("caller line")?;
        let nested = u32::try_from(line.find("leaf()").ok_or("nested call")?)?;
        let direct = u32::try_from(line.rfind("leaf()").ok_or("direct call")?)?;
        let mut call = item(location(directory.path(), "lib.rs", 0, 7, 11), "leaf");
        call.from_ranges = vec![
            Range::new(Position::new(1, nested), Position::new(1, nested + 4)),
            Range::new(Position::new(1, direct), Position::new(1, direct + 4)),
        ];
        let held = super::CallGraphSource {
            provider: rift_protocol::read::EdgeProvider::Engine,
            trees: [directory.path(), directory.path()],
            source,
            unit: crate::read::source_unit_id(&ProjectPath::new("lib.rs")?),
            direction: rift_protocol::read::TraversalDirection::Outgoing,
        };
        let expected = u64::try_from(source.rfind("leaf()").ok_or("direct call")?)?;
        for (name, ranges, start) in [
            ("root", call.from_ranges.clone(), Some(expected)),
            ("root", vec![call.from_ranges[0]], None),
            (
                "nested",
                call.from_ranges.clone(),
                Some(u64::try_from(source.find("leaf();").ok_or("nested call")?)?),
            ),
        ] {
            let seed = rift_core::SymbolId::new(symbol(&reads, name).0)?;
            let mut observed = call.clone();
            observed.from_ranges = ranges;
            let graph = super::map_graph_calls(
                &reads,
                &CalleeRoots::default(),
                &seed,
                CalleeReport {
                    calls: Ok(vec![observed]),
                    encoding: PositionEncoding::Utf16,
                },
                &held,
            )?;
            if let Some(start) = start {
                let [(_, sites)] = graph.local.as_slice() else {
                    return Err("one callee".into());
                };
                assert_eq!(
                    sites.len(),
                    1,
                    "a nested closure owns its own call: {sites:?}"
                );
                assert_eq!(sites[0].range.start, start);
            } else {
                assert!(
                    graph.local.is_empty(),
                    "a filtered site is not an unlocated neighbor"
                );
            }
            assert!(graph.complete);
        }

        Ok(())
    }

    const INCOMING_CALLERS_SOURCE: &str = r"pub fn leaf() {}

pub fn only_nested() {
    let nested = || { leaf(); };
    nested();
}

pub fn both() {
    let inner = || { leaf(); };
    inner();
    leaf();
}

pub fn direct() {
    leaf();
}

pub fn siblings() {
    let first = || { leaf(); };
    let second = || { leaf(); };
    first();
    second();
}

pub fn unlocated() {}
";

    #[test]
    fn incoming_graph_calls_keep_the_physical_caller_of_each_site() -> TestResult {
        use rift_protocol::read::TraversalDirection;

        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let source = INCOMING_CALLERS_SOURCE;
        fs::write(root.join("lib.rs"), source)?;
        let reads = reads(root)?;
        let target = symbol(&reads, "leaf");
        let seed = rift_core::SymbolId::new(target.as_str())?;
        let unit = crate::read::source_unit_id(&ProjectPath::new("lib.rs")?);
        // Rust-analyzer groups a named closure into its enclosing function's call item.
        let nested_site = Range::new(Position::new(3, 22), Position::new(3, 26));
        let inner_site = Range::new(Position::new(8, 21), Position::new(8, 25));
        let both_site = Range::new(Position::new(10, 4), Position::new(10, 8));
        let direct_site = Range::new(Position::new(14, 4), Position::new(14, 8));
        let first_site = Range::new(Position::new(18, 21), Position::new(18, 25));
        let second_site = Range::new(Position::new(19, 22), Position::new(19, 26));
        let mut only_nested = item(location(root, "lib.rs", 2, 7, 18), "only_nested");
        only_nested.from_ranges = vec![nested_site];
        let mut both = item(location(root, "lib.rs", 7, 7, 11), "both");
        both.from_ranges = vec![inner_site, both_site];
        let mut direct = item(location(root, "lib.rs", 13, 7, 13), "direct");
        direct.from_ranges = vec![direct_site];
        let mut siblings = item(location(root, "lib.rs", 17, 7, 15), "siblings");
        siblings.from_ranges = vec![first_site, second_site];
        let mut crossing = siblings.clone();
        crossing.from_ranges = vec![Range::new(first_site.start, second_site.end)];
        let mut unrelated = direct.clone();
        unrelated.from_ranges = vec![nested_site];
        let mut empty = direct.clone();
        empty.from_ranges = vec![Range::new(direct_site.start, direct_site.start)];
        let missing = item(location(root, "missing.rs", 0, 7, 14), "missing");
        let unlocated = item(location(root, "lib.rs", 24, 7, 16), "unlocated");
        let held = super::CallGraphSource {
            trees: [root, root],
            source,
            unit: unit.clone(),
            direction: TraversalDirection::Incoming,
            provider: EdgeProvider::Lsp,
        };
        let index = rift_lsp::LineIndex::new(source);
        let mut expected = BTreeMap::from([(symbol(&reads, "unlocated"), Vec::new())]);
        for (caller, range) in [
            ("nested", nested_site),
            ("inner", inner_site),
            ("both", both_site),
            ("direct", direct_site),
            ("first", first_site),
            ("second", second_site),
        ] {
            let start = index.byte_offset(PositionEncoding::Utf16, range.start)?;
            let end = index.byte_offset(PositionEncoding::Utf16, range.end)?;
            assert_eq!(source.get(start..end), Some("leaf"));
            expected.insert(
                symbol(&reads, caller),
                vec![rift_protocol::read::SourceUnitSpan {
                    unit: unit.clone(),
                    range: rift_protocol::read::TextRange {
                        start: u64::try_from(start)?,
                        end: u64::try_from(end)?,
                    },
                }],
            );
        }
        let valid = [only_nested, both, direct, siblings, unlocated];
        for omitted in [
            None,
            Some(crossing),
            Some(unrelated),
            Some(empty),
            Some(missing),
        ] {
            let complete = omitted.is_none();
            let calls = valid.iter().cloned().chain(omitted).collect();
            let answer = super::map_graph_calls(
                &reads,
                &CalleeRoots::default(),
                &seed,
                CalleeReport {
                    calls: Ok(calls),
                    encoding: PositionEncoding::Utf16,
                },
                &held,
            )?;
            assert_eq!(
                answer.local.into_iter().collect::<BTreeMap<_, _>>(),
                expected
            );
            assert_eq!(answer.edges.len(), expected.len());
            for edge in answer.edges {
                assert_eq!(edge.to, target);
                assert_eq!(edge.provider, EdgeProvider::Lsp);
                let sites = expected.get(&edge.from).ok_or("stored call owner")?;
                assert_eq!(edge.occurrence.as_ref(), sites.first());
            }
            assert_eq!(answer.complete, complete);
        }
        Ok(())
    }

    /// Each callee maps to the declaration holding its name when the tree indexes that
    /// file; a vendored stub and a file below an installed package's root wait for the
    /// global API, and a file outside every root and a file the tree does not index are
    /// counted instead, one per call.
    #[test]
    fn callee_mapping_holds_project_and_package_callees_and_counts_the_rest() -> TestResult {
        let directory = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let installed = tempfile::tempdir()?;
        let source = "pub fn beacon() {}\n\npub fn larger() { beacon(); }\n";
        fs::write(directory.path().join("lib.rs"), source)?;
        let reads = reads(directory.path())?;
        let beacon = location(directory.path(), "lib.rs", 0, 7, 13);
        let stub = Location {
            uri: "vendored://stdlib/json/__init__.pyi".parse()?,
            range: beacon.range,
        };
        let package = rift_core::PackageIdentity {
            manager: "cargo".to_owned(),
            registry: "crates.io".to_owned(),
            name: "serde".to_owned(),
            version: "1.0.228".to_owned(),
        };
        let roots = CalleeRoots::from_packages(vec![rift_lsp::uri::PackageRoot::new(
            TreeRoot::new(installed.path())?,
            package.clone(),
        )]);
        let calls = vec![
            item(beacon, "beacon"),
            item(stub, "loads"),
            item(
                location(installed.path(), "src/de.rs", 4, 11, 22),
                "deserialize",
            ),
            item(location(outside.path(), "lib.rs", 0, 7, 13), "beacon"),
            item(location(directory.path(), "absent.rs", 0, 0, 1), "absent"),
        ];
        let report = CalleeReport {
            calls: Ok(calls),
            encoding: PositionEncoding::Utf16,
        };
        let caller = rift_core::SymbolId::new(symbol(&reads, "larger").0)?;
        let mapped = map_callees(
            &reads,
            report,
            &caller,
            (&roots, &[directory.path()]),
            EdgeProvider::Lsp,
            None,
        )?;
        assert_eq!(mapped.callees, [symbol(&reads, "beacon")]);
        assert_eq!(mapped.dropped, 2);
        let held: Vec<(String, String, u32, u32)> = mapped
            .held
            .iter()
            .map(|callee| {
                (
                    format!("{}/{}", callee.package().manager(), callee.package().name()),
                    callee.path().as_str().to_owned(),
                    callee.position().line,
                    callee.position().character,
                )
            })
            .collect();
        assert_eq!(
            held,
            [
                (
                    "stdlib/python".to_owned(),
                    "json/__init__.pyi".to_owned(),
                    0,
                    7
                ),
                ("cargo/serde".to_owned(), "src/de.rs".to_owned(), 4, 11),
            ]
        );
        assert!(
            mapped
                .held
                .iter()
                .all(|callee| callee.encoding() == PositionEncoding::Utf16)
        );
        Ok(())
    }

    /// A callee range the served bytes do not hold, and an answer past the syntax element bound, are
    /// the engine answer faults that degrade a walk rather than refuse it.
    #[test]
    fn callee_mapping_names_an_unmappable_answer_an_engine_answer_fault() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let past_the_line = location(directory.path(), "lib.rs", 0, 80, 81);
        let caller = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        for calls in [
            Ok(vec![item(past_the_line.clone(), "beacon")]),
            Err(super::TRAVERSAL_SYMBOLS_MAX + 1),
        ] {
            let report = CalleeReport {
                calls,
                encoding: PositionEncoding::Utf16,
            };
            let roots = CalleeRoots::default();
            let Err(error) = map_callees(
                &reads,
                report,
                &caller,
                (&roots, &[directory.path()]),
                EdgeProvider::Lsp,
                None,
            ) else {
                panic!("an unmappable answer is refused");
            };
            assert!(
                error.slug() == errors::server::read_engine_answer::SLUG,
                "{error}"
            );
        }
        Ok(())
    }

    /// One callee a walk holds for the global API, from a typeshed stub the embedded
    /// engine names.
    fn held_stub(
        stub: &str,
        name: &str,
    ) -> Result<crate::callee::PackageCallee, Box<dyn std::error::Error>> {
        let caller = rift_core::SymbolId::new("rift://symbol/python/app.py/hello")?;
        let uri: lsp_types::Uri = format!("vendored://stdlib/{stub}").parse()?;
        let call = crate::callee::NamedCallee {
            provider: EdgeProvider::Engine,
            uri: &uri,
            name,
            position: Position {
                line: 1,
                character: 4,
            },
            encoding: PositionEncoding::Utf8,
        };
        match crate::callee::callee_file(&CalleeRoots::default(), &[], &caller, call)? {
            crate::callee::CalleeFile::Package(held) => Ok(held),
            _ => Err("a typeshed stub is held for the global API".into()),
        }
    }

    /// A named callee ends one edge per declaration however many calls named it, carries
    /// the package declaration its hit reads, and every callee named nothing drops.
    #[test]
    fn named_package_callees_end_one_edge_each_and_the_rest_drop() -> TestResult {
        let caller = rift_core::SymbolId::new("rift://symbol/python/app.py/hello")?;
        let len = SymbolId("rift://symbol/stdlib/cpython/python/builtins/len".into());
        let python = rift_protocol::read::RuntimeIdentity {
            runtime: "cpython".to_owned(),
            version: "3.12.9".to_owned(),
        };
        let mut references = EngineReferences {
            package_callees: vec![
                held_stub("builtins.pyi", "len")?,
                held_stub("builtins.pyi", "len")?,
                held_stub("json/__init__.pyi", "loads")?,
            ],
            ..EngineReferences::default()
        };
        assert_eq!(references.package_callees().len(), 3);
        references.name_package_callees(|callee| {
            (callee.path().as_str() == "builtins.pyi").then(|| CalleeDeclaration {
                id: len.clone(),
                kind: ExactKind("function".to_owned()),
                origin: rift_core::SourceLocation::Stdlib {
                    runtime: Some(python.clone()),
                },
                revision: python.release().expect("runtime release"),
            })
        });
        let edges: Vec<&SymbolId> = references
            .outgoing(&caller)
            .iter()
            .map(|edge| &edge.edge.to)
            .collect();
        assert_eq!(edges, [&len], "one edge per callee declaration");
        assert_eq!(references.dropped_callees, 1, "`loads` is named nothing");
        assert!(references.package_callees().is_empty());
        let declaration = references
            .package_declaration(&rift_core::SymbolId::new(len.0.clone())?)
            .ok_or("the edge's end is a package declaration")?;
        assert_eq!(
            declaration.unit.0,
            "rift://source/stdlib/cpython/builtins.pyi"
        );
        assert_eq!(declaration.symbol.name, "len");
        assert_eq!(declaration.symbol.kind.0, "function", "the stored kind");

        references.package_callees = vec![held_stub("json/__init__.pyi", "loads")?];
        references.drop_package_callees();
        assert_eq!(references.dropped_callees, 2);
        assert!(references.package_callees().is_empty());

        references.package_callees = vec![held_stub("builtins.pyi", "len")?];
        references.degrade(super::engine_analysis_warning(
            rift_protocol::read::Language {
                name: "python".to_owned(),
                dialect: None,
            },
            "dropped",
        ));
        assert!(references.package_callees().is_empty());
        assert!(
            references
                .package_declaration(&rift_core::SymbolId::new(len.0)?)
                .is_none(),
            "a dropped contribution takes its package declarations with it"
        );
        Ok(())
    }

    /// The engine tier's warnings ride in wire order, and a dropped contribution takes
    /// what its answers said about callees and readiness with it.
    #[test]
    fn engine_warnings_name_dropped_callees_and_unconfirmed_engines_until_degraded() {
        let mut references = EngineReferences {
            dropped_callees: 2,
            unconfirmed: ["rust".to_owned(), "python".to_owned()].into(),
            ..EngineReferences::default()
        };
        let wire = serde_json::to_value(references.warnings()).expect("warnings serialize");
        assert_eq!(wire[0]["code"], "callees_dropped", "{wire}");
        assert_eq!(wire[0]["callees"], 2, "{wire}");
        assert_eq!(wire[1]["code"], "engine_readiness_unconfirmed", "{wire}");
        assert_eq!(wire[1]["processes"], json!(["python", "rust"]), "{wire}");
        assert!(
            wire[1]["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("`python`, `rust`")),
            "{wire}"
        );

        references.degrade(super::engine_analysis_warning(
            super::Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            "character out of range",
        ));
        let wire = serde_json::to_value(references.warnings()).expect("warnings serialize");
        assert_eq!(
            wire.as_array().map(Vec::len),
            Some(1),
            "only the dropped contribution is named: {wire}"
        );
        assert_eq!(wire[0]["code"], "engine_analysis_unavailable", "{wire}");
        assert!(EngineReferences::default().warnings().is_empty());
    }

    #[test]
    fn unresolved_registry_roots_keep_one_warning_when_engine_answers_are_dropped() {
        let mut references = EngineReferences {
            registry_unresolved: true,
            ..EngineReferences::default()
        };
        let warnings = references.warnings();
        assert_eq!(warnings.len(), 1);
        assert!(
            matches!(&warnings[0], rift_protocol::read::ReadWarning::PackageContextDegraded { resolver, reason } if resolver == "cargo" && reason.len() < 256)
        );
        references.degrade(super::engine_analysis_warning(
            super::Language {
                name: "rust".to_owned(),
                dialect: None,
            },
            "character out of range",
        ));
        let warnings = references.warnings();
        assert_eq!(warnings.len(), 2);
        assert!(
            matches!(&warnings[1], rift_protocol::read::ReadWarning::PackageContextDegraded { resolver, .. } if resolver == "cargo")
        );
    }

    /// An outgoing walk keeps the refusals an incoming walk draws: beside `rev`, beside
    /// `change`, and with `scope: "global"`.
    #[test]
    fn an_outgoing_walk_keeps_the_incoming_walk_refusals() {
        let seed = crate::traversal::tests::graph_identity("beacon");
        for (extra, slug, context_key, context_value) in [
            (
                json!({"revision": "rift://revision/git/main"}),
                errors::server::read_unsupported::SLUG,
                "capability",
                "traversal at a revision",
            ),
            (
                json!({"change": {"base": "baseline"}}),
                errors::server::read_unsupported::SLUG,
                "capability",
                crate::search::CHANGE_TRAVERSAL_CAPABILITY,
            ),
            (
                json!({"scope": "global"}),
                errors::server::read_invalid::SLUG,
                "violation",
                "the edge graph serves the project alone",
            ),
        ] {
            for direction in ["incoming", "outgoing"] {
                let mut request = json!({"traversal": {"seed": seed, "direction": direction}});
                for (key, value) in extra.as_object().expect("an object") {
                    request[key] = value.clone();
                }
                let params: SearchParams =
                    serde_json::from_value(request.clone()).expect("search request");
                let refused =
                    crate::search::validate_search(&params).expect_err("the walk refuses");
                assert_eq!(refused.slug(), slug, "{request}: {refused}");
                assert!(
                    refused
                        .context()
                        .any(|(key, value)| key == context_key && value == context_value),
                    "{request}: {refused}"
                );
            }
        }
    }

    fn reads(root: &std::path::Path) -> Result<ReadService, crate::RiftError> {
        let manifest = root.join("Cargo.toml");
        fs::write(
            &manifest,
            "[package]\nname='beacon'\nversion='1.0.0'\n[lib]\npath='lib.rs'\n",
        )
        .map_err(|source| {
            errors::index::workspace_filesystem()
                .path(&manifest)
                .source(source)
                .error()
        })?;
        ReadService::build(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
        )
    }

    fn python_module_metadata(root: &std::path::Path, modules: &str) -> std::io::Result<()> {
        fs::write(
            root.join("pyproject.toml"),
            format!(
                "[build-system]\nrequires=['setuptools==80.9.0']\nbuild-backend='setuptools.build_meta'\n[tool.setuptools]\npy-modules={modules}\n"
            ),
        )
    }

    fn symbol(reads: &ReadService, name: &str) -> SymbolId {
        let params: GetSymbolParams =
            serde_json::from_value(json!({"name": name})).expect("lookup");
        reads.get_symbol(&params).expect("lookup answer").hits[0]
            .symbol
            .id
            .clone()
            .expect("symbol identity")
    }

    fn request(seed: &SymbolId) -> SearchParams {
        serde_json::from_value(json!({"target":"symbol","traversal":{"seed":seed,"direction":"incoming","kinds":["references"]}})).expect("traversal request")
    }

    fn location(root: &std::path::Path, path: &str, line: u32, start: u32, end: u32) -> Location {
        let tree = TreeRoot::new(root).expect("tree root");
        let path = ProjectPath::new(path).expect("project path");
        Location {
            uri: tree.document_uri(&path).expect("document URI"),
            range: Range {
                start: Position {
                    line,
                    character: start,
                },
                end: Position {
                    line,
                    character: end,
                },
            },
        }
    }

    fn pool(root: &std::path::Path, language: &str, configuration: LspConfiguration) -> EnginePool {
        let key = LspProcessKey::named("test");
        EnginePool::new(
            root,
            BTreeMap::from([(key.clone(), configuration)]),
            BTreeMap::from([(language.to_owned(), key)]),
        )
    }

    #[test]
    fn exact_name_range_ignores_attributes_comments_and_strings() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source =
            "#[beacon]\n/// beacon\npub fn /* beacon */ beacon() { let text = \"beacon\"; }\n";
        fs::write(directory.path().join("lib.rs"), source)?;
        let reads = reads(directory.path())?;
        let file = reads
            .index()
            .file(&ProjectPath::new("lib.rs")?)
            .expect("indexed file");
        let declaration = file
            .syntax()
            .symbols()
            .iter()
            .find(|symbol| symbol.name == "beacon")
            .expect("declaration");
        let offset = declaration_name_offset(file, declaration)?;
        assert_eq!(offset, source.find("beacon()").expect("name token"));
        let mut invalid = declaration.clone();
        invalid.name_range.as_mut().expect("name range").end = u64::MAX;
        assert!(declaration_name_offset(file, &invalid).is_err());
        invalid.name_range = None;
        assert!(declaration_name_offset(file, &invalid).is_err());
        Ok(())
    }

    #[test]
    fn definition_mapping_requires_a_declared_name() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn caller() -> u32 { 0 }\n",
        )?;
        let reads = reads(directory.path())?;
        let path = ProjectPath::new("lib.rs")?;
        let location = Range::new(Position::new(0, 19), Position::new(0, 22));
        assert!(
            super::project_definition(&reads, &path, (location, PositionEncoding::Utf8), false)?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn graph_reference_mapping_keeps_local_locations_beside_outside_locations() -> TestResult {
        let directory = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() {}\npub fn caller() { beacon(); }\n",
        )?;
        let reads = reads(directory.path())?;
        let target = symbol(&reads, "beacon");
        let report = ReferenceReport {
            definitions: BTreeMap::from([(
                super::reference_key(&location(directory.path(), "lib.rs", 1, 18, 24)),
                vec![location(directory.path(), "lib.rs", 0, 7, 13)],
            )])
            .into(),
            locations: Ok(vec![
                location(outside.path(), "other.rs", 0, 0, 3),
                location(directory.path(), "lib.rs", 0, 7, 13),
                location(directory.path(), "not_indexed.rs", 0, 0, 3),
                location(directory.path(), "lib.rs", 1, 18, 24),
            ]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let answer = super::map_inventory_references(
            &reads,
            &target,
            report,
            directory.path(),
            true,
            EdgeProvider::Lsp,
        )?;
        assert!(answer.complete);
        assert_eq!(answer.occurrences.len(), 1);
        assert_eq!(
            answer.occurrences[0].enclosing,
            Some(symbol(&reads, "caller"))
        );
        assert_eq!(answer.occurrences[0].target, target);
        Ok(())
    }

    /// typescript-language-server names a quoted property key without its quotes, among
    /// the references of that key and as its definition.
    #[test]
    fn a_location_inside_the_declared_name_is_not_a_reference_to_it() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("headers.js"),
            "function value() { return 1; }\nvar headers = {\n  'etag': value()\n};\n",
        )?;
        let reads = reads(directory.path())?;
        let target = symbol(&reads, "'etag'");
        let key = location(directory.path(), "headers.js", 2, 3, 7);
        let report = ReferenceReport {
            definitions: BTreeMap::from([(super::reference_key(&key), vec![key.clone()])]).into(),
            locations: Ok(vec![key]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let answer = super::map_inventory_references(
            &reads,
            &target,
            report,
            directory.path(),
            false,
            EdgeProvider::Lsp,
        )?;
        assert!(
            answer.occurrences.is_empty(),
            "the key's own name is no reference: {:?}",
            answer.occurrences
        );
        assert!(answer.complete);
        Ok(())
    }

    #[test]
    fn inventory_references_follow_definitions_for_two_seeds_in_one_file() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn first() {}\npub fn second() {}\npub fn caller() { first(); second(); unknown(); }\n",
        )?;
        let reads = reads(directory.path())?;
        let first = symbol(&reads, "first");
        let second = symbol(&reads, "second");
        let locations = vec![
            location(directory.path(), "lib.rs", 2, 18, 23),
            location(directory.path(), "lib.rs", 2, 27, 33),
            location(directory.path(), "lib.rs", 2, 37, 44),
        ];
        let definitions = BTreeMap::from([
            (
                super::reference_key(&locations[0]),
                vec![location(directory.path(), "lib.rs", 0, 7, 12)],
            ),
            (
                super::reference_key(&locations[1]),
                vec![location(directory.path(), "lib.rs", 1, 7, 13)],
            ),
        ]);
        // A server can return the same structural-property occurrences for both seeds.
        let mut mapping = super::ReferenceMapping::new(
            &reads,
            directory.path(),
            PositionEncoding::Utf8,
            EdgeProvider::Lsp,
        )?;
        for (target, start) in [(first, 55), (second, 64)] {
            let report = ReferenceReport {
                definitions: definitions.clone().into(),
                locations: Ok(locations.clone()),
                encoding: PositionEncoding::Utf8,
                full: true,
            };
            let answer =
                super::map_inventory_references_with_mapping(&target, report, true, &mut mapping)?;
            assert_eq!(answer.occurrences.len(), 1, "{answer:?}");
            assert_eq!(answer.occurrences[0].target, target);
            assert_eq!(answer.occurrences[0].source.range.start, start);
            assert!(!answer.complete);
            assert_eq!(answer.edge_warnings.len(), 1);
        }
        assert_eq!(
            mapping.line_index_builds, 1,
            "one captured file is indexed once across seeds"
        );
        assert_eq!(
            mapping.definition_resolutions, 2,
            "each returned definition is resolved once across seeds"
        );
        Ok(())
    }

    #[test]
    fn reference_mapping_retains_cross_file_callers_and_deduplicates() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() {}\nmod caller;\n",
        )?;
        fs::write(
            directory.path().join("caller.rs"),
            "pub fn caller() { beacon(); beacon(); }\n",
        )?;
        let reads = reads(directory.path())?;
        let seed = symbol(&reads, "beacon");
        let target = rift_core::SymbolId::new(seed.0.clone())?;
        let own = location(directory.path(), "lib.rs", 0, 7, 13);
        let caller = location(directory.path(), "caller.rs", 0, 18, 24);
        let other = location(directory.path(), "caller.rs", 0, 28, 34);
        let canonical = location(reads.index().root(), "caller.rs", 0, 18, 24);
        let report = ReferenceReport {
            definitions: BTreeMap::new().into(),
            locations: Ok(vec![own, caller.clone(), other, caller, canonical]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let edges = map_references(&reads, &target, report, directory.path(), EdgeProvider::Lsp)?;
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].edge.from, symbol(&reads, "caller"));
        assert_eq!(edges[0].edge.to, seed);
        assert_eq!(
            edges[0].edge.kinds,
            vec![rift_protocol::read::EdgeKind::References]
        );
        Ok(())
    }

    #[test]
    fn reference_mapping_refuses_invalid_ranges_and_location_overflow() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let seed = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        for (start, end) in [(80, 81), (10, 2)] {
            let report = ReferenceReport {
                definitions: BTreeMap::new().into(),
                locations: Ok(vec![location(directory.path(), "lib.rs", 0, start, end)]),
                encoding: PositionEncoding::Utf16,
                full: true,
            };
            assert!(
                map_references(&reads, &seed, report, directory.path(), EdgeProvider::Lsp).is_err()
            );
        }
        let overflow = ReferenceReport {
            definitions: BTreeMap::new().into(),
            locations: Err(super::TRAVERSAL_SYMBOLS_MAX + 1),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        assert!(
            map_references(&reads, &seed, overflow, directory.path(), EdgeProvider::Lsp).is_err()
        );
        Ok(())
    }

    #[test]
    fn reference_mapping_skips_unindexed_files_and_refuses_non_file_uris() -> TestResult {
        let directory = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let target = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let report = ReferenceReport {
            definitions: BTreeMap::new().into(),
            locations: Ok(vec![
                location(outside.path(), "lib.rs", 0, 0, 1),
                location(directory.path(), "absent.rs", 0, 0, 1),
            ]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        assert!(
            map_references(&reads, &target, report, directory.path(), EdgeProvider::Lsp)?
                .is_empty()
        );
        let mut refused = location(directory.path(), "lib.rs", 0, 0, 1);
        refused.uri = "https://example.com/lib.rs".parse()?;
        let report = ReferenceReport {
            definitions: BTreeMap::new().into(),
            locations: Ok(vec![refused]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let error = map_references(&reads, &target, report, directory.path(), EdgeProvider::Lsp)
            .expect_err("scheme refused");
        assert_eq!(error.slug(), errors::lsp::uri_scheme_refused::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| { key == "scheme" && value == "https" })
        );
        Ok(())
    }

    /// An unmappable answer degrades; every other mapping failure still
    /// refuses, because it says nothing about what revision the engine holds.
    #[test]
    fn only_an_unmappable_answer_degrades_the_engine_contribution() {
        let language = super::Language {
            name: "rust".to_owned(),
            dialect: None,
        };
        assert!(matches!(
            super::symbol_references(Ok(Vec::new()), language.clone(), None),
            Ok(super::SymbolReferences::Resolved { edges, .. }) if edges.is_empty()
        ));
        let unmapped = super::symbol_references(
            errors::server::read_engine_answer()
                .operation("engine references")
                .detail("character out of range")
                .fail(),
            language.clone(),
            None,
        );
        assert!(matches!(
            unmapped,
            Ok(super::SymbolReferences::Unmapped { detail, .. })
                if detail.contains("character out of range")
        ));
        let refused = super::symbol_references(
            errors::server::read_task()
                .operation("reference URI conversion")
                .detail("scheme refused")
                .fail(),
            language,
            None,
        );
        assert!(
            refused.is_err(),
            "a failure that is not the engine's revision still refuses the read"
        );
    }

    #[tokio::test]
    async fn reference_sources_require_indexed_declaration_name_ranges() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("settings.toml"), "beacon = 7\n")?;
        let reads = reads(directory.path())?;
        let configuration = serde_json::from_value(json!({"command":"/refused-engine"}))?;
        let engines = pool(directory.path(), "toml", configuration);
        let params: GetSymbolParams = serde_json::from_value(json!({"name": "beacon"}))?;
        assert_eq!(
            reads.get_symbol(&params)?.hits[0]
                .symbol
                .id
                .as_ref()
                .map(|id| id.0.as_str()),
            Some("rift://symbol/local/toml/settings.toml/beacon")
        );
        let language = rift_protocol::read::Language::from_identity_segment("toml")?;
        let identity = |name: &str| {
            rift_protocol::identity::SymbolIdentity::new(
                rift_protocol::identity::SymbolOwner::Local,
                language.clone(),
                vec!["settings.toml".to_owned(), name.to_owned()],
            )
            .map(|identity| identity.wire_identity())
        };
        let target = rift_core::SymbolId::new(identity("beacon")?)?;
        let file = reads
            .index()
            .file(&ProjectPath::new("settings.toml")?)
            .expect("indexed file");
        assert!(
            file.syntax()
                .symbols()
                .iter()
                .all(|symbol| symbol.name_range.is_none())
        );
        assert!(matches!(
            Box::pin(super::resolve_symbol_references(
                &reads,
                &engines,
                &target,
                far()
            ))
            .await?,
            super::SymbolReferences::NotServed
        ));
        let absent = rift_core::SymbolId::new(identity("absent")?)?;
        assert!(matches!(
            Box::pin(super::resolve_symbol_references(
                &reads,
                &engines,
                &absent,
                far()
            ))
            .await?,
            super::SymbolReferences::NotServed
        ));
        assert_eq!(
            engines.state_for_key(&LspProcessKey::named("test")),
            Some(rift_protocol::workspace::LspState::Stopped)
        );
        Ok(())
    }

    #[tokio::test]
    async fn embedded_references_resolve_two_incoming_depths() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("main.py"),
            "def beacon() -> int:\n    return 7\n\ndef caller() -> int:\n    return beacon()\n\ndef outer() -> int:\n    return caller()\n",
        )?;
        python_module_metadata(directory.path(), "['main']")?;
        let reads = reads(directory.path())?;
        let configuration = serde_json::from_value(
            json!({"embedded":"ty","retry":{"attempts":2,"delay":"1ms","delay_limit":"1ms"}}),
        )?;
        let engines = pool(directory.path(), "python", configuration);
        let mut params = request(&symbol(&reads, "beacon"));
        params.traversal.as_mut().expect("traversal").depth = 2;
        let result = Box::pin(resolve_engine_references(&reads, &engines, &params, walk())).await;
        engines.shutdown().await;
        let references = result?;
        let seed = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let caller = rift_core::SymbolId::new(symbol(&reads, "caller").0)?;
        assert_eq!(references.incoming(&seed)[0].edge.from.0, caller.as_str());
        assert_eq!(
            references.incoming(&caller)[0].edge.from,
            symbol(&reads, "outer")
        );
        let answer =
            reads.search_with_references(&params, &StoreAnswer::identifier_only(), &references)?;
        assert_eq!(answer.results.len(), 2);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn engine_reference_locations_are_bounded_before_deduplication() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let count = super::TRAVERSAL_SYMBOLS_MAX + 1;
        let locations = vec![location(directory.path(), "lib.rs", 0, 7, 13); count];
        let locations = serde_json::to_string(&locations)?;
        let script = directory.path().join("edge_responses.sh");
        let engine = r#"locations='$EDGE_LOCATIONS'
frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=${header%?}; length=${length##*[!0-9]}
  body=$(dd bs=1 count="$length" 2>/dev/null)
  case "$body" in *'"id":'*) id=${body#*\"id\":}; id=${id%%[!0-9]*} ;; *) id= ;; esac
  case "$body" in
    *'"method":"initialize"'*) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{\"referencesProvider\":true}}}" ;;
    *'"method":"exit"'*) break ;;
    *'"method":"textDocument/references"'*) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":$locations}" ;;
    *'"id":'*) frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
  esac
done
"#.replace("$EDGE_LOCATIONS", &locations);
        fs::write(&script, engine)?;
        let mut fixture = super::process_lifecycle::retrying(
            super::process_lifecycle::answers(&[], &["rust"]),
            2,
        );
        fixture.configuration.command = Some(
            rift_protocol::configuration::CommandInput::ProgramAndArguments(vec![
                "sh".to_owned(),
                script.to_string_lossy().into_owned(),
            ]),
        );
        let engines = pool(directory.path(), "rust", fixture.configuration);
        let result = Box::pin(resolve_engine_references(
            &reads,
            &engines,
            &request(&symbol(&reads, "beacon")),
            walk(),
        ))
        .await;
        engines.shutdown().await;
        let references = result?;
        assert!(
            references.is_empty(),
            "an answer past the bound contributes no edge"
        );
        let Some(rift_protocol::read::ReadWarning::EngineAnalysisUnavailable { language, detail }) =
            references.analysis_unavailable()
        else {
            panic!("an oversized engine answer warns: {references:?}");
        };
        assert_eq!(
            language.as_ref().map(|language| language.name.as_str()),
            Some("rust")
        );
        assert!(
            detail.contains(&format!("reference locations {count}")),
            "{detail}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn reference_edge_bound_applies_across_engine_requests() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("main.py"),
            "def beacon() -> int:\n    return 7\n\ndef caller() -> int:\n    return beacon()\n",
        )?;
        python_module_metadata(directory.path(), "['main']")?;
        let reads = reads(directory.path())?;
        let target = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let prior = rift_core::SymbolId::new(symbol(&reads, "caller").0)?;
        let report = ReferenceReport {
            definitions: BTreeMap::new().into(),
            locations: Ok(vec![location(directory.path(), "main.py", 4, 11, 17)]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let edge =
            map_references(&reads, &target, report, directory.path(), EdgeProvider::Lsp)?.remove(0);
        let prior_edges = (0..super::TRAVERSAL_SYMBOLS_MAX)
            .map(|index| {
                let mut edge = edge.clone();
                edge.edge.from = SymbolId(format!("rift://symbol/python/prior.py/caller_{index}"));
                edge.edge.to = SymbolId(prior.as_str().to_owned());
                edge
            })
            .collect();
        let mut references =
            EngineReferences::from_incoming(BTreeMap::from([(prior.clone(), prior_edges)]));
        let configuration = serde_json::from_value(
            json!({"embedded":"ty","retry":{"attempts":2,"delay":"1ms","delay_limit":"1ms"}}),
        )?;
        let engines = pool(directory.path(), "python", configuration);
        let mut requested = std::collections::BTreeSet::from([prior]);
        let result = Box::pin(super::extend_references(
            &reads,
            &engines,
            &mut references,
            vec![target.clone()],
            &mut requested,
            far(),
        ))
        .await;
        engines.shutdown().await;
        let Some(rift_protocol::read::ReadWarning::EngineAnalysisUnavailable { detail, .. }) =
            result?
        else {
            panic!("the cumulative edge bound warns");
        };
        assert!(
            detail.contains("more incoming references than the"),
            "{detail}"
        );
        assert!(references.incoming(&target).is_empty());
        Ok(())
    }

    #[test]
    fn references_reject_another_publication() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let references = EngineReferences {
            revision: Some("other".to_owned()),
            ..EngineReferences::default()
        };
        let refused = reads
            .search_with_references(
                &request(&symbol(&reads, "beacon")),
                &StoreAnswer::identifier_only(),
                &references,
            )
            .expect_err("references from another publication are refused");
        assert_eq!(refused.slug(), errors::server::read_unavailable::SLUG);
        assert!(
            refused
                .context()
                .any(|(key, value)| { key == "operation" && value == "engine references" })
        );
        assert!(refused.context().any(|(key, value)| {
            key == "detail" && value == "source revision changed before search"
        }));
        Ok(())
    }

    #[tokio::test]
    async fn no_selected_engine_refuses_the_walk() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() {}\npub fn caller() { beacon(); }\n",
        )?;
        let reads = reads(directory.path())?;
        let params = request(&symbol(&reads, "beacon"));
        let engines = EnginePool::new(directory.path(), BTreeMap::new(), BTreeMap::new());
        assert!(!super::uses_engine_references(&reads, &engines, &params)?);
        let references =
            Box::pin(resolve_engine_references(&reads, &engines, &params, walk())).await?;
        assert!(references.is_empty());
        let refused = reads
            .search_with_references(&params, &StoreAnswer::identifier_only(), &references)
            .expect_err("no engine answered, so the walk has no edge source");
        assert_eq!(refused.slug(), errors::server::read_unsupported::SLUG);
        assert!(refused.context().any(|(key, value)| {
            key == "capability" && value == crate::traversal::TRAVERSAL_CAPABILITY
        }));
        Ok(())
    }

    #[tokio::test]
    async fn configured_engine_failure_preserves_its_error() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let configuration = serde_json::from_value(
            json!({"command":"/refused-engine","retry":{"attempts":1},"restart":{"attempts":0}}),
        )?;
        let engines = pool(directory.path(), "rust", configuration);
        let result = Box::pin(resolve_engine_references(
            &reads,
            &engines,
            &request(&symbol(&reads, "beacon")),
            walk(),
        ))
        .await;
        engines.shutdown().await;
        let error = result.expect_err("absolute program is refused");
        assert_eq!(error.slug(), errors::lsp::engine_program_absolute::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| { key == "program" && value == "/refused-engine" })
        );
        Ok(())
    }

    #[tokio::test]
    async fn embedded_engine_resolves_cross_file_reference_without_writes() -> TestResult {
        let directory = tempfile::tempdir()?;
        let declaration = "def beacon() -> int:\n    return 7\n";
        let caller = "from helper import beacon\n\ndef caller() -> int:\n    return beacon()\n";
        fs::write(directory.path().join("helper.py"), declaration)?;
        fs::write(directory.path().join("main.py"), caller)?;
        python_module_metadata(directory.path(), "['main','helper']")?;
        let reads = reads(directory.path())?;
        let configuration = serde_json::from_value(
            json!({"embedded":"ty","retry":{"attempts":2,"delay":"1ms","delay_limit":"1ms"}}),
        )?;
        let engines = pool(directory.path(), "python", configuration);
        let params = request(&symbol(&reads, "beacon"));
        let resolved = Box::pin(resolve_engine_references(&reads, &engines, &params, walk())).await;
        engines.shutdown().await;
        let references = resolved?;
        assert!(
            !references.is_empty(),
            "engine must contribute the cross-file reference"
        );
        let answer =
            reads.search_with_references(&params, &StoreAnswer::identifier_only(), &references)?;
        assert!(
            answer
                .results
                .iter()
                .any(|hit| hit.path.as_ref().is_some_and(|path| path.0 == "main.py"))
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("helper.py"))?,
            declaration
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("main.py"))?,
            caller
        );
        Ok(())
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn absent_references_capability_refuses_the_walk() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() {}\npub fn caller() { beacon(); }\n",
        )?;
        let reads = reads(directory.path())?;
        let params = request(&symbol(&reads, "beacon"));
        let mut fixture = super::process_lifecycle::answers(&[], &["rust"]);
        let rift_protocol::configuration::CommandInput::ProgramAndArguments(command) = fixture
            .configuration
            .command
            .as_mut()
            .expect("process command")
        else {
            panic!("fixture uses arguments");
        };
        command[2] = command[2].replace("referencesProvider\":true", "referencesProvider\":null");
        // true and null have identical byte lengths, so the fixture's frame stays valid.
        let engines = pool(directory.path(), "rust", fixture.configuration);
        let result = Box::pin(resolve_engine_references(&reads, &engines, &params, walk())).await;
        engines.shutdown().await;
        let references = result?;
        assert!(references.is_empty());
        let refused = reads
            .search_with_references(&params, &StoreAnswer::identifier_only(), &references)
            .expect_err("an engine without the references capability answers no edge");
        assert_eq!(refused.slug(), errors::server::read_unsupported::SLUG);
        assert!(refused.context().any(|(key, value)| {
            key == "capability" && value == crate::traversal::TRAVERSAL_CAPABILITY
        }));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn engine_refusal_remains_a_typed_read_failure() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let response = super::process_lifecycle::refused_response(1, -32602, "references refused");
        let mut fixture = super::process_lifecycle::answers(&[response], &["rust"]);
        fixture.retry.attempts = 1;
        let engines = pool(directory.path(), "rust", fixture.configuration);
        let result = Box::pin(resolve_engine_references(
            &reads,
            &engines,
            &request(&symbol(&reads, "beacon")),
            walk(),
        ))
        .await;
        engines.shutdown().await;
        let error = result.expect_err("engine refused references");
        assert_eq!(error.slug(), errors::lsp::engine_refused_terminal::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| { key == "method" && value == "textDocument/references" })
        );
        assert!(
            error
                .context()
                .any(|(key, value)| { key == "code" && value == "-32602" })
        );
        assert!(
            error
                .context()
                .any(|(key, value)| { key == "message" && value == "references refused" })
        );
        Ok(())
    }
    #[tokio::test]
    async fn invalid_search_refuses_before_selected_engine_launch() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let configuration = serde_json::from_value(json!({"command":"/refused-engine"}))?;
        let engines = pool(directory.path(), "rust", configuration);
        let base = request(&symbol(&reads, "beacon"));
        for invalid in [
            json!({"scope":"global"}),
            json!({"query":""}),
            json!({"max_results":0}),
            json!({"paths":{"include":["["]}}),
        ] {
            let mut value = serde_json::to_value(&base)?;
            value
                .as_object_mut()
                .expect("request")
                .extend(invalid.as_object().expect("overrides").clone());
            let params = serde_json::from_value(value)?;
            let expected = reads
                .search(&params, &StoreAnswer::identifier_only())
                .expect_err("invalid search");
            let error = Box::pin(resolve_engine_references(&reads, &engines, &params, walk()))
                .await
                .expect_err("invalid search before engine");
            assert_eq!(error.slug(), expected.slug());
            assert_eq!(
                error.context().collect::<Vec<_>>(),
                expected.context().collect::<Vec<_>>()
            );
            assert_eq!(error.detail(), expected.detail());
        }
        engines.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn resolved_empty_references_answer_an_empty_walk() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("helper.py"),
            "def beacon() -> int:\n    return 7\n",
        )?;
        python_module_metadata(directory.path(), "['helper']")?;
        let reads = ReadService::build_with_languages(
            directory.path(),
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            &rift_core::LanguageFileSelections::default(),
            HistoryConfiguration::default(),
            rift_protocol::dependencies::DependenciesConfiguration::default(),
        )?;
        let configuration = serde_json::from_value(
            json!({"embedded":"ty","retry":{"attempts":2,"delay":"1ms","delay_limit":"1ms"}}),
        )?;
        let engines = pool(directory.path(), "python", configuration);
        let params = request(&symbol(&reads, "beacon"));
        let result = Box::pin(resolve_engine_references(&reads, &engines, &params, walk())).await;
        engines.shutdown().await;
        let references = result?;
        assert!(references.is_empty());
        let seed = params
            .traversal
            .as_ref()
            .and_then(|traversal| traversal.seed.as_ref())
            .expect("the request names a seed");
        assert!(references.resolved(seed));
        assert!(
            reads
                .search_with_references(&params, &StoreAnswer::identifier_only(), &references)?
                .results
                .is_empty()
        );
        Ok(())
    }
    #[test]
    fn engine_eligibility_preserves_indexed_only_searches() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let configuration = serde_json::from_value(json!({"command":"/refused-engine"}))?;
        let engines = pool(directory.path(), "rust", configuration);
        let base = request(&symbol(&reads, "beacon"));
        assert!(super::uses_engine_references(&reads, &engines, &base)?);
        let mut calls = base.clone();
        calls.traversal.as_mut().expect("traversal").kinds =
            vec![rift_protocol::read::EdgeKind::Calls];
        let mut files = base;
        files.target = rift_protocol::read::SearchParamsTarget::File;
        for params in [calls, files] {
            assert!(!super::uses_engine_references(&reads, &engines, &params)?);
        }
        assert_eq!(
            engines.state_for_key(&LspProcessKey::named("test")),
            Some(rift_protocol::workspace::LspState::Stopped)
        );
        Ok(())
    }

    #[test]
    fn depth_two_reaches_engine_configured_caller_of_unconfigured_seed() -> TestResult {
        let store = crate::traversal::tests::call_graph_store();
        let seed = rift_core::SymbolId::new(crate::traversal::tests::graph_identity("leaf"))?;
        let caller = rift_core::SymbolId::new(crate::traversal::tests::graph_identity("branch_a"))?;
        let mut traversal = request(&SymbolId(seed.as_str().to_owned()))
            .traversal
            .expect("traversal");
        traversal.kinds.clear();
        let selected = |identity: &rift_core::SymbolId| identity == &caller;
        assert!(!selected(&seed));
        assert!(!super::reachable_reference_source(
            &store,
            &seed,
            &traversal,
            |identity| Ok(selected(identity))
        )?);
        traversal.depth = 2;
        assert!(super::reachable_reference_source(
            &store,
            &seed,
            &traversal,
            |identity| Ok(selected(identity))
        )?);
        traversal.kinds = vec![rift_protocol::read::EdgeKind::References];
        assert!(!super::reachable_reference_source(
            &store,
            &seed,
            &traversal,
            |identity| Ok(selected(identity))
        )?);
        Ok(())
    }

    /// A `sh` engine answering each request from its method alone: `initialize` with the
    /// capabilities `CAPABILITIES` spells, a call hierarchy prepare through the `PREPARE`
    /// step, outgoing calls through the `OUTGOING` step, `exit` by exiting, and every other
    /// request, `shutdown` included, with `null`.
    #[cfg(unix)]
    const METHOD_ENGINE: &str = r#"frame() { printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"; }
while IFS= read -r header; do
  IFS= read -r blank
  length=${header%?}; length=${length##*[!0-9]}
  body=$(dd bs=1 count="$length" 2>/dev/null)
  case "$body" in *'"id":'*) id=${body#*\"id\":}; id=${id%%[!0-9]*} ;; *) id= ;; esac
  case "$body" in *'"uri":"'*) uri=${body#*\"uri\":\"}; uri=${uri%%\"*} ;; *) uri= ;; esac
  case "$body" in
    *'"method":"initialize"'*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":CAPABILITIES}}" ;;
    *'"method":"textDocument/prepareCallHierarchy"'*)
      PREPARE ;;
    *'"method":"callHierarchy/outgoingCalls"'*)
      OUTGOING ;;
    *'"method":"exit"'*)
      exit 0 ;;
    *'"id":'[0-9]*)
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}" ;;
  esac
done
"#;

    #[cfg(unix)]
    #[tokio::test]
    async fn graph_references_keep_only_captured_engine_files() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() {}\nfn caller() { beacon(); }\n",
        )?;
        let reads = reads(directory.path())?;
        let locations = vec![
            location(directory.path(), "lib.rs", 1, 14, 20),
            location(directory.path(), "excluded.rs", 0, 0, 6),
        ];
        let payload = serde_json::to_string(&locations)?.replace('"', "\\\"");
        let definition = serde_json::to_string(&[location(directory.path(), "lib.rs", 0, 7, 13)])?
            .replace('"', "\\\"");
        let step = format!(
            "    *'\"method\":\"textDocument/references\"'*)\n      frame \"{{\\\"jsonrpc\\\":\\\"2.0\\\",\\\"id\\\":$id,\\\"result\\\":{payload}}}\" ;;\n    *'\"method\":\"textDocument/definition\"'*)\n      frame \"{{\\\"jsonrpc\\\":\\\"2.0\\\",\\\"id\\\":$id,\\\"result\\\":{definition}}}\" ;;\n    *'\"method\":\"exit\"'*)"
        );
        let script = directory.path().join("engine.sh");
        let text = METHOD_ENGINE
            .replace(
                "CAPABILITIES",
                r#"{\"referencesProvider\":true,\"definitionProvider\":true}"#,
            )
            .replace("PREPARE", ":")
            .replace("OUTGOING", ":")
            .replace("    *'\"method\":\"exit\"'*)", &step);
        fs::write(&script, text)?;
        let configuration = serde_json::from_value(json!({
            "command": ["sh", script.display().to_string()],
            "settle_delay": "1ms",
            "retry": {"attempts": 2, "delay": "1ms", "delay_limit": "1ms"}
        }))?;
        let engines = pool(directory.path(), "rust", configuration);
        let target = symbol(&reads, "beacon");
        let result = Box::pin(super::graph_references(
            &reads,
            &engines,
            &target,
            true,
            far(),
        ))
        .await;
        engines.shutdown().await;
        let answer = result?;
        assert!(answer.complete, "{answer:?}");
        assert_eq!(answer.occurrences.len(), 1, "{answer:?}");
        assert_eq!(
            answer.occurrences[0].enclosing,
            Some(symbol(&reads, "caller"))
        );
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn graph_calls_restore_only_independently_confirmed_callee_sites() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn leaf() {}\npub fn other() {}\npub fn root() { leaf(); }\n",
        )?;
        let reads = reads(directory.path())?;
        let uri = location(directory.path(), "lib.rs", 2, 7, 11).uri;
        let range = json!({"start":{"line":2,"character":7},"end":{"line":2,"character":11}});
        let prepared =
            json!([{"name":"root","kind":12,"uri":uri,"range":range,"selectionRange":range}]);
        let mut outgoing = item(location(directory.path(), "lib.rs", 1, 7, 12), "other");
        outgoing
            .from_ranges
            .push(Range::new(Position::new(2, 16), Position::new(2, 20)));
        let response = |value: &serde_json::Value| -> Result<String, Box<dyn std::error::Error>> {
            let payload = serde_json::to_string(value)?.replace('"', "\\\"");
            Ok(format!(
                "frame \"{{\\\"jsonrpc\\\":\\\"2.0\\\",\\\"id\\\":$id,\\\"result\\\":{payload}}}\""
            ))
        };
        for confirmed in [false, true] {
            let references = if confirmed {
                serde_json::to_value([location(directory.path(), "lib.rs", 2, 16, 20)])?
            } else {
                json!([])
            };
            let reference_step = format!(
                "    *'\"method\":\"textDocument/references\"'*)\n      references_read=1\n      {} ;;\n    *'\"method\":\"textDocument/definition\"'*)\n      if [ \"$references_read\" = 1 ]; then {} ; else {} ; fi ;;\n    *'\"method\":\"exit\"'*)",
                response(&references)?,
                response(&serde_json::to_value([location(
                    directory.path(),
                    "lib.rs",
                    0,
                    7,
                    11
                )])?)?,
                response(&serde_json::to_value([location(
                    directory.path(),
                    "lib.rs",
                    1,
                    7,
                    12
                )])?)?,
            );
            let text = METHOD_ENGINE
                .replace(
                    "CAPABILITIES",
                    r#"{\"referencesProvider\":true,\"definitionProvider\":true,\"callHierarchyProvider\":true}"#,
                )
                .replace("PREPARE", &response(&prepared)?)
                .replace("OUTGOING", &response(&serde_json::to_value([&outgoing])?)?)
                .replace("    *'\"method\":\"exit\"'*)", &reference_step);
            let engine = tempfile::tempdir()?;
            let script = engine.path().join("engine.sh");
            fs::write(&script, text)?;
            let configuration = serde_json::from_value(json!({
                "command":["sh",script.display().to_string()], "settle_delay":"1ms",
                "retry":{"attempts":2,"delay":"1ms","delay_limit":"1ms"}
            }))?;
            let engines = pool(directory.path(), "rust", configuration);
            let result = Box::pin(super::graph_calls(
                &reads,
                &engines,
                &CalleeRoots::default(),
                &symbol(&reads, "root"),
                rift_protocol::read::TraversalDirection::Outgoing,
                far(),
            ))
            .await;
            engines.shutdown().await;
            let graph = result?;
            assert!(
                !graph.complete,
                "the original disagreement remains incomplete"
            );
            assert_eq!(
                graph
                    .local
                    .iter()
                    .any(|(target, _)| target == &symbol(&reads, "leaf")),
                confirmed
            );
            assert_eq!(
                graph.edges.iter().any(|edge| {
                    edge.from == symbol(&reads, "root")
                        && edge.to == symbol(&reads, "leaf")
                        && edge.kinds == [rift_protocol::read::EdgeKind::Calls]
                        && edge.provider == EdgeProvider::Lsp
                }),
                confirmed,
                "a confirmed call retains the provider edge: {:?}",
                graph.edges
            );
            assert!(graph.corroboration.iter().any(|record| !record.agrees));
            assert_eq!(
                graph.corroboration.iter().any(|record| record.agrees),
                confirmed
            );
        }
        Ok(())
    }

    /// The range [`METHOD_ENGINE`] names every item at, `beacon` on line 0.
    #[cfg(unix)]
    const METHOD_ENGINE_RANGE: &str =
        r#"{\"start\":{\"line\":0,\"character\":7},\"end\":{\"line\":0,\"character\":13}}"#;

    /// The capabilities of a [`METHOD_ENGINE`] serving call hierarchy.
    #[cfg(unix)]
    const CALL_HIERARCHY: &str = r#"{\"callHierarchyProvider\":true}"#;

    /// The `PREPARE` step of a [`METHOD_ENGINE`] preparing one `beacon` item in the
    /// requested document.
    #[cfg(unix)]
    const PREPARED_BEACON: &str = r#"frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"name\":\"beacon\",\"kind\":12,\"uri\":\"$uri\",\"range\":RANGE,\"selectionRange\":RANGE}]}""#;

    /// The steps one [`METHOD_ENGINE`] takes: the capabilities it advertises, and how it
    /// answers a prepare and an outgoing calls request.
    #[cfg(unix)]
    struct MethodSteps<'step> {
        capabilities: &'step str,
        prepare: &'step str,
        outgoing: &'step str,
    }

    /// A pool running [`METHOD_ENGINE`] for Rust over `root` under `steps`, its script
    /// written below `engine`.
    #[cfg(unix)]
    fn method_engine(
        root: &std::path::Path,
        engine: &std::path::Path,
        steps: &MethodSteps<'_>,
    ) -> Result<EnginePool, Box<dyn std::error::Error>> {
        let script = engine.join("engine.sh");
        let text = METHOD_ENGINE
            .replace("CAPABILITIES", steps.capabilities)
            .replace("PREPARE", steps.prepare)
            .replace("OUTGOING", steps.outgoing)
            .replace("RANGE", METHOD_ENGINE_RANGE);
        fs::write(&script, text)?;
        let table = json!({
            "command": ["sh", script.display().to_string()],
            "settle_delay": "10ms",
            "retry": {"attempts": 2, "delay": "1ms", "delay_limit": "1ms"},
        });
        let configuration = serde_json::from_value(table)?;
        Ok(pool(root, "rust", configuration))
    }

    #[cfg(unix)]
    fn outgoing_request(seed: &SymbolId) -> SearchParams {
        serde_json::from_value(json!({"target":"symbol","traversal":{"seed":seed,"direction":"outgoing","kinds":["calls"]}})).expect("traversal request")
    }

    /// An outgoing walk skips a declaration it already asked about and one no engine
    /// serves: a declaration the index does not hold, and one whose engine advertises no
    /// call hierarchy. None of them draws an edge or a warning.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_outgoing_walk_skips_requested_and_unserved_declarations() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source = "pub fn beacon() {}\npub fn caller() { beacon(); }\n";
        fs::write(directory.path().join("lib.rs"), source)?;
        let reads = reads(directory.path())?;
        let engine = tempfile::tempdir()?;
        let references_only = r#"{\"referencesProvider\":true}"#;
        let steps = MethodSteps {
            capabilities: references_only,
            prepare: ":",
            outgoing: ":",
        };
        let engines = method_engine(directory.path(), engine.path(), &steps)?;
        let beacon = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let caller = rift_core::SymbolId::new(symbol(&reads, "caller").0)?;
        let absent = rift_core::SymbolId::new(crate::traversal::tests::graph_identity("absent"))?;
        let mut references = EngineReferences::default();
        let mut requested = std::collections::BTreeSet::from([caller.clone()]);
        let (deadline, roots) = walk();
        let walk = super::OutgoingWalk {
            seed: &caller,
            deadline,
            roots,
        };
        let pending = vec![caller.clone(), absent.clone(), beacon.clone()];
        let extended = Box::pin(super::extend_callees(
            &reads,
            &engines,
            &mut references,
            pending,
            &mut requested,
            walk,
        ))
        .await;
        engines.shutdown().await;
        assert_eq!(extended?, None);
        assert!(references.is_empty(), "{references:?}");
        assert!(references.warnings().is_empty(), "{references:?}");
        assert_eq!(
            requested,
            std::collections::BTreeSet::from([absent, beacon, caller])
        );
        Ok(())
    }

    /// One declaration's outgoing calls past the syntax element bound drop the engine's whole
    /// contribution, with the warning naming the count, before any callee is mapped.
    #[cfg(unix)]
    #[tokio::test]
    async fn outgoing_calls_past_the_symbol_bound_drop_the_engine_contribution() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let engine = tempfile::tempdir()?;
        let count = super::TRAVERSAL_SYMBOLS_MAX + 1;
        let range = json!({"start":{"line":0,"character":7},"end":{"line":0,"character":13}});
        let callee = json!({"name":"beacon","kind":12,"uri":"file:///elsewhere/lib.rs","range":range,"selectionRange":range});
        let calls = engine.path().join("calls.json");
        let answer = vec![json!({"to": callee, "fromRanges": []}); count];
        fs::write(&calls, serde_json::to_string(&answer)?)?;
        let outgoing = r#"prefix="{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":"
      size=$(wc -c < 'CALLS')
      printf 'Content-Length: %d\r\n\r\n%s' "$(( ${#prefix} + $size + 1 ))" "$prefix"
      cat 'CALLS'
      printf '}'"#
            .replace("CALLS", &calls.display().to_string());
        let steps = MethodSteps {
            capabilities: CALL_HIERARCHY,
            prepare: PREPARED_BEACON,
            outgoing: &outgoing,
        };
        let engines = method_engine(directory.path(), engine.path(), &steps)?;
        let params = outgoing_request(&symbol(&reads, "beacon"));
        let result = Box::pin(resolve_engine_references(&reads, &engines, &params, walk())).await;
        engines.shutdown().await;
        let references = result?;
        assert!(
            references.is_empty(),
            "an answer past the bound adds no edge"
        );
        let warning = serde_json::to_value(references.analysis_unavailable())?;
        assert_eq!(warning["code"], "engine_analysis_unavailable", "{warning}");
        let expected = format!("outgoing calls {count} exceed");
        let detail = warning["detail"].as_str().unwrap_or_default();
        assert!(detail.contains(&expected), "{detail}");
        Ok(())
    }

    /// An outgoing calls answer outside the method's shape refuses the walk with the
    /// engine's error. The engine closed its input before it answered, so closing the
    /// document after the exchange fails too; the close failure is logged, and the
    /// refusal stands.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_outgoing_answer_outside_the_method_shape_refuses_the_walk() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let engine = tempfile::tempdir()?;
        let outgoing = r#"exec 0<&-
      frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":7}""#;
        let steps = MethodSteps {
            capabilities: CALL_HIERARCHY,
            prepare: PREPARED_BEACON,
            outgoing,
        };
        let engines = method_engine(directory.path(), engine.path(), &steps)?;
        let params = outgoing_request(&symbol(&reads, "beacon"));
        let result = Box::pin(resolve_engine_references(&reads, &engines, &params, walk())).await;
        engines.shutdown().await;
        let error = result.expect_err("an answer outside the shape refuses the walk");
        assert_eq!(error.slug(), errors::lsp::engine_result_invalid::SLUG);
        assert!(
            error
                .context()
                .any(|(key, value)| { key == "method" && value == "callHierarchy/outgoingCalls" })
        );
        assert!(
            std::error::Error::source(&error)
                .and_then(|source| source.downcast_ref::<serde_json::Error>())
                .is_some()
        );
        Ok(())
    }

    /// A callee URI the address rules refuse, a `file` URI naming a host, refuses the walk
    /// as the server's own failure rather than dropping the engine's contribution: it says
    /// nothing about the revision the engine read.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_callee_uri_the_rules_refuse_refuses_the_walk() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
        let reads = reads(directory.path())?;
        let engine = tempfile::tempdir()?;
        let outgoing = r#"frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":[{\"to\":{\"name\":\"beacon\",\"kind\":12,\"uri\":\"file://build-host/ws/lib.rs\",\"range\":RANGE,\"selectionRange\":RANGE},\"fromRanges\":[]}]}""#;
        let steps = MethodSteps {
            capabilities: CALL_HIERARCHY,
            prepare: PREPARED_BEACON,
            outgoing,
        };
        let engines = method_engine(directory.path(), engine.path(), &steps)?;
        let params = outgoing_request(&symbol(&reads, "beacon"));
        let result = Box::pin(resolve_engine_references(&reads, &engines, &params, walk())).await;
        engines.shutdown().await;
        let error = result.expect_err("a refused callee URI refuses the walk");
        assert!(error.slug() == errors::server::read_task::SLUG, "{error}");
        Ok(())
    }

    /// A declaration past the seed that the engine prepares no call hierarchy item at keeps
    /// an empty callee list, where the seed would refuse.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unprepared_declaration_past_the_seed_keeps_an_empty_callee_list() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source = "pub fn beacon() {}\npub fn caller() { beacon(); }\n";
        fs::write(directory.path().join("lib.rs"), source)?;
        let reads = reads(directory.path())?;
        let engine = tempfile::tempdir()?;
        let steps = MethodSteps {
            capabilities: CALL_HIERARCHY,
            prepare: r#"frame "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":null}""#,
            outgoing: ":",
        };
        let engines = method_engine(directory.path(), engine.path(), &steps)?;
        let caller = rift_core::SymbolId::new(symbol(&reads, "caller").0)?;
        let beacon = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let mut references = EngineReferences::default();
        let mut requested = std::collections::BTreeSet::new();
        let (deadline, roots) = walk();
        let walk = super::OutgoingWalk {
            seed: &caller,
            deadline,
            roots,
        };
        let extended = Box::pin(super::extend_callees(
            &reads,
            &engines,
            &mut references,
            vec![beacon.clone()],
            &mut requested,
            walk,
        ))
        .await;
        engines.shutdown().await;
        assert_eq!(extended?, None);
        assert_eq!(references.outgoing.get(&beacon), Some(&Vec::new()));
        Ok(())
    }

    /// The outgoing edge bound counts the edges earlier requests of the same walk stored:
    /// a request whose callees pass what is left drops the engine contribution with a
    /// warning.
    #[tokio::test]
    async fn callee_edge_bound_applies_across_engine_requests() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source =
            "def beacon() -> int:\n    return 7\n\ndef caller() -> int:\n    return beacon()\n";
        fs::write(directory.path().join("main.py"), source)?;
        python_module_metadata(directory.path(), "['main']")?;
        let reads = reads(directory.path())?;
        let caller = rift_core::SymbolId::new(symbol(&reads, "caller").0)?;
        let prior = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let prior_edges = (0..super::TRAVERSAL_SYMBOLS_MAX)
            .map(|index| {
                let end = SymbolId(format!("rift://symbol/python/prior.py/callee_{index}"));
                super::call_hop(
                    &SymbolId(prior.as_str().to_owned()),
                    end,
                    EdgeProvider::Engine,
                )
            })
            .collect();
        let mut references = EngineReferences {
            outgoing: BTreeMap::from([(prior.clone(), prior_edges)]),
            ..EngineReferences::default()
        };
        let table =
            json!({"embedded":"ty","retry":{"attempts":2,"delay":"1ms","delay_limit":"1ms"}});
        let configuration = serde_json::from_value(table)?;
        let engines = pool(directory.path(), "python", configuration);
        let mut requested = std::collections::BTreeSet::from([prior]);
        let (deadline, roots) = walk();
        let walk = super::OutgoingWalk {
            seed: &caller,
            deadline,
            roots,
        };
        let extended = Box::pin(super::extend_callees(
            &reads,
            &engines,
            &mut references,
            vec![caller.clone()],
            &mut requested,
            walk,
        ))
        .await;
        engines.shutdown().await;
        let warning = serde_json::to_value(extended?)?;
        assert_eq!(warning["code"], "engine_analysis_unavailable", "{warning}");
        let detail = warning["detail"].as_str().unwrap_or_default();
        assert!(detail.contains("more outgoing calls than the"), "{detail}");
        assert!(references.outgoing(&caller).is_empty());
        Ok(())
    }
}
