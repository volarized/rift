//! Relationship traversal through configured language engines: incoming references,
//! and outgoing calls through call hierarchy.

use std::collections::{BTreeMap, BTreeSet};

use lsp_types::{CallHierarchyItem, Location, Position, Range, Uri};
use rift_core::{ProjectPath, SymbolId as CoreSymbolId};
use rift_index::{IndexedFile, RelationshipStore};
use rift_lsp::capabilities::PositionEncoding;
use rift_lsp::position::LineIndex;
use rift_lsp::session::{EngineError, EngineFault, EngineSession};
use rift_lsp::uri::{TreeRoot, UriFault};
use rift_protocol::read::{
    ExactKind, Extensions, GraphHop, HopDirection, Language, ReadWarning, Relationship,
    RelationshipDerivation, RelationshipFacet, SearchParams, SearchParamsTarget, SearchTraversal,
    SourceUnitId, Symbol, SymbolId, TraversalDirection,
};
use rift_syntax::SyntaxSymbol;
use tokio::time::Instant;

use crate::callee::{
    CalleeDeclaration, CalleeFile, CalleeRoots, NamedCallee, PackageCallee, callee_file,
};
use crate::engine::{EnginePool, EngineSlot, OutgoingAnswer, SessionFuture};
use crate::read::{ReadError, ReadFault, ReadService, symbol_id};
use crate::traversal::{
    TRAVERSAL_NODES_MAX, engine_facet, resolve_graph_symbol, validate_traversal,
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
    /// contribution, the callees an outgoing walk dropped, then the engines it took
    /// unconfirmed. Empty when every consulted engine answered in full.
    #[must_use]
    pub fn warnings(&self) -> Vec<ReadWarning> {
        let mut warnings: Vec<ReadWarning> = self.analysis_unavailable.iter().cloned().collect();
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
    /// for this read are dropped with the rest. The indexed relationships answer, and
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
            let declaration = named(&callee).and_then(|declaration| {
                Some(PackageDeclaration {
                    symbol: callee.symbol(&declaration)?,
                    unit: callee.unit(&declaration.package)?,
                })
            });
            let Some(declaration) = declaration else {
                self.dropped_callees = self.dropped_callees.saturating_add(1);
                continue;
            };
            let Some(id) = declaration.symbol.id.clone() else {
                continue;
            };
            let Ok(end) = CoreSymbolId::new(id.0.clone()) else {
                self.dropped_callees = self.dropped_callees.saturating_add(1);
                continue;
            };
            let caller = SymbolId(callee.caller().as_str().to_owned());
            let edges = self.outgoing.entry(callee.caller().clone()).or_default();
            if !edges.iter().any(|edge| edge.relationship.to == id) {
                edges.push(call_hop(&caller, id));
                edges.sort_by(|left, right| left.relationship.to.cmp(&right.relationship.to));
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

    pub(crate) fn validate_revision(&self, reads: &ReadService) -> Result<(), ReadError> {
        if self
            .revision
            .as_deref()
            .is_some_and(|revision| revision != reads.tree_revision())
        {
            return Err(ReadFault::unavailable(
                "engine references",
                "source revision changed before search",
            ));
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
) -> Result<bool, ReadError> {
    reads.validate_engine_search(params)?;
    let Some((traversal, seed)) = reference_traversal(params) else {
        return Ok(false);
    };
    validate_traversal(traversal)?;
    let seed = CoreSymbolId::new(seed.0.clone())
        .map_err(|_| ReadFault::invalid("traversal.seed", "not a symbol identity"))?;
    Ok(reachable_reference_source(
        reads.relationships(),
        &seed,
        traversal,
        |identity| reference_source(reads, engines, identity).is_some(),
    ))
}

fn reachable_reference_source(
    store: &RelationshipStore,
    seed: &CoreSymbolId,
    traversal: &SearchTraversal,
    has_source: impl Fn(&CoreSymbolId) -> bool,
) -> bool {
    if has_source(seed) {
        return true;
    }
    if traversal.depth == 1 {
        return false;
    }
    let mut prior = traversal.clone();
    prior.depth -= 1;
    walk_traversal_with_references(
        store,
        seed,
        &prior,
        TRAVERSAL_NODES_MAX,
        &EngineReferences::default(),
    )
    .discovered
    .into_iter()
    .any(|(identity, _)| has_source(&identity))
}

/// The traversal a configured engine may answer for, with the declaration it starts at:
/// references for an incoming walk, calls for an outgoing one. A walk riding beside
/// `change` names no `seed` and reaches no engine: both compared sides are committed
/// revisions, which no engine session serves.
fn reference_traversal(params: &SearchParams) -> Option<(&SearchTraversal, &SymbolId)> {
    let traversal = params.traversal.as_ref()?;
    let seed = traversal.seed.as_ref()?;
    let current = params.rev.is_none();
    let symbols = matches!(
        params.target,
        SearchParamsTarget::All | SearchParamsTarget::Symbol
    );
    let served = traversal.facets.is_empty()
        || traversal
            .facets
            .contains(&engine_facet(traversal.direction));
    (current && symbols && served).then_some((traversal, seed))
}

fn reference_source<'source>(
    reads: &'source ReadService,
    engines: &'source EnginePool,
    identity: &CoreSymbolId,
) -> Option<(
    &'source EngineSlot,
    &'source IndexedFile,
    &'source SyntaxSymbol,
)> {
    let (file, symbol) = resolve_graph_symbol(reads.index(), identity)?;
    symbol.name_range?;
    let slot = engines.engine_for(file.syntax().language())?;
    Some((slot, file, symbol))
}

/// Resolves incoming references or outgoing calls while retaining every indexed
/// relationship.
///
/// Only current-tree symbol traversals that name no facet, or name the engine facet of
/// their direction, consult engines: `references` incoming, `calls` outgoing.
/// The traversal's existing depth and node bounds also bound engine requests. Each
/// exchange uses the selected engine's retry and restart policy. The caller applies
/// its request deadline and validates the captured tree again before using the result.
/// Engines without references support leave the indexed answer unchanged.
///
/// An engine answering about bytes the served revision does not carry, or answering more
/// than the traversal bound carries, has its whole contribution dropped: the result holds
/// the indexed relationships and an `engine_analysis_unavailable` warning naming what is
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
/// Dropping the future discards the selected session and its open documents.
/// No source file is written.
pub async fn resolve_engine_references(
    reads: &ReadService,
    engines: &EnginePool,
    params: &SearchParams,
    (deadline, roots): (Instant, &CalleeRoots),
) -> Result<EngineReferences, ReadError> {
    if !uses_engine_references(reads, engines, params)? {
        return Ok(EngineReferences::default());
    }
    let Some((traversal, seed)) = reference_traversal(params) else {
        return Ok(EngineReferences::default());
    };
    validate_traversal(traversal)?;
    let seed = CoreSymbolId::new(seed.0.clone())
        .map_err(|_| ReadFault::invalid("traversal.seed", "not a symbol identity"))?;
    let mut references = EngineReferences {
        revision: Some(reads.tree_revision().to_owned()),
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
            reads.relationships(),
            &seed,
            &step,
            TRAVERSAL_NODES_MAX,
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
) -> Result<Option<ReadWarning>, ReadError> {
    let stored: usize = references.incoming.values().map(Vec::len).sum();
    let mut remaining = TRAVERSAL_NODES_MAX - stored;
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
                     {TRAVERSAL_NODES_MAX}-node traversal bound carries"
                ),
            )));
        };
        remaining = left;
        references.incoming.insert(identity, edges);
    }
    if !references.unconfirmed.is_empty() {
        tracing::info!(
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
    /// caller asked for relationships, not for an engine.
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
fn callee_hops(seed: &SymbolId, callees: impl IntoIterator<Item = SymbolId>) -> Vec<GraphHop> {
    callees
        .into_iter()
        .filter(|callee| callee != seed)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|callee| call_hop(seed, callee))
        .collect()
}

/// The outgoing edge from `from` to the declaration `to` it calls.
fn call_hop(from: &SymbolId, to: SymbolId) -> GraphHop {
    GraphHop {
        relationship: Relationship {
            from: from.clone(),
            to,
            kind: ExactKind(rift_core::fault_label(&RelationshipFacet::Calls)),
            facets: vec![RelationshipFacet::Calls],
            evidence: Vec::new(),
            derivation: RelationshipDerivation::Resolution,
            confidence: None,
            extensions: Extensions::default(),
        },
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
) -> Result<Option<ReadWarning>, ReadError> {
    let stored: usize = references.outgoing.values().map(Vec::len).sum::<usize>()
        + references.package_callees.len();
    let mut remaining = TRAVERSAL_NODES_MAX - stored;
    for identity in pending {
        if !requested.insert(identity.clone()) {
            continue;
        }
        let resolved = resolve_symbol_callees(reads, engines, &identity, walk).await?;
        let (edges, held, language) = match resolved {
            SymbolCallees::Resolved {
                callees,
                held,
                dropped,
                language,
                unconfirmed,
            } => {
                references.dropped_callees = references.dropped_callees.saturating_add(dropped);
                references.unconfirmed.extend(unconfirmed);
                let seed = SymbolId(identity.as_str().to_owned());
                (callee_hops(&seed, callees), held, language)
            }
            SymbolCallees::NotServed => continue,
            SymbolCallees::Unprepared { kind } if identity == *walk.seed => {
                return Err(ReadFault::unsupported(format!(
                    "outgoing traversal from a declaration of kind `{kind}` (the language \
                     engine prepares no call hierarchy item at the seed)"
                )));
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
                     {TRAVERSAL_NODES_MAX}-node traversal bound carries"
                ),
            )));
        };
        remaining = left;
        references.outgoing.insert(identity, edges);
        references.package_callees.extend(held);
    }
    if !references.unconfirmed.is_empty() || references.dropped_callees > 0 {
        tracing::info!(
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
        /// Callee declarations, one per call the engine named, repeats included.
        callees: Vec<SymbolId>,
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
) -> Result<SymbolCallees, ReadError> {
    let Some((slot, file, symbol)) = reference_source(reads, engines, identity) else {
        return Ok(SymbolCallees::NotServed);
    };
    let language = file.syntax().language().clone();
    let target = ReferenceTarget::new(slot.workspace_root(), reads.index().root(), file, symbol)?;
    let answered = Box::pin(callees_on_engine(slot, &target, walk.deadline)).await;
    let (report, unconfirmed) = match answered {
        Ok(OutgoingAnswer::Ready(report)) => (report, None),
        Ok(OutgoingAnswer::Unconfirmed(report)) => (report, Some(slot.name().to_owned())),
        Ok(OutgoingAnswer::Unprepared) => {
            return Ok(SymbolCallees::Unprepared { kind: symbol.kind });
        }
        Ok(OutgoingAnswer::Unsettled { attempts }) => {
            return Ok(SymbolCallees::Unsettled { language, attempts });
        }
        Err(error) if matches!(error.fault(), EngineFault::CapabilityAbsent { .. }) => {
            return Ok(SymbolCallees::NotServed);
        }
        Err(error) => return Err(ReadFault::engine(error)),
    };
    let trees = [slot.workspace_root(), reads.index().root()];
    match map_callees(reads, report, identity, (walk.roots, &trees)) {
        Ok(MappedCallees {
            callees,
            held,
            dropped,
        }) => Ok(SymbolCallees::Resolved {
            callees,
            held,
            dropped,
            language,
            unconfirmed,
        }),
        Err(error) if matches!(error.fault(), ReadFault::EngineAnswer { .. }) => {
            Ok(SymbolCallees::Unmapped {
                language,
                detail: error.detail(),
            })
        }
        Err(error) => Err(error),
    }
}

/// One outgoing calls answer: each callee the engine named, or the count past the
/// traversal bound.
struct CalleeReport {
    calls: Result<Vec<CallHierarchyItem>, usize>,
    encoding: PositionEncoding,
}

async fn callees_on_engine(
    slot: &EngineSlot,
    target: &ReferenceTarget,
    deadline: Instant,
) -> Result<OutgoingAnswer<CalleeReport>, EngineError> {
    let request_path = target.path.clone();
    let utf8 = target.utf8;
    let utf16 = target.utf16;
    slot.request_outgoing(
        target.opening(),
        move |session: &mut EngineSession| {
            let path = request_path.clone();
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
                let mut calls = Vec::new();
                for item in items {
                    let answered = session.outgoing_calls(item).await?;
                    calls.extend(answered.into_iter().map(|call| call.to));
                    if calls.len() > TRAVERSAL_NODES_MAX {
                        return Ok(Some(CalleeReport {
                            calls: Err(calls.len()),
                            encoding,
                        }));
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

/// What one outgoing calls answer maps to.
struct MappedCallees {
    /// The project declarations holding each callee's name, one per call.
    callees: Vec<SymbolId>,
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
) -> Result<MappedCallees, ReadError> {
    let calls = report.calls.map_err(|observed| {
        ReadFault::engine_answer(
            "engine calls",
            format!(
                "outgoing calls {observed} exceed the {TRAVERSAL_NODES_MAX}-node traversal bound"
            ),
        )
    })?;
    let mut mapped = MappedCallees {
        callees: Vec::new(),
        held: Vec::new(),
        dropped: 0,
    };
    for item in &calls {
        let call = NamedCallee {
            uri: &item.uri,
            name: &item.name,
            position: item.selection_range.start,
            encoding: report.encoding,
        };
        let file = callee_file(roots, trees, caller, call)
            .map_err(|error| ReadFault::task("callee URI conversion", error.detail()))?;
        let declaration = match file {
            CalleeFile::Project(path) => project_declaration(
                reads,
                &path,
                (item.selection_range, report.encoding),
                "engine calls",
            )?,
            CalleeFile::Package(held) => {
                mapped.held.push(held);
                continue;
            }
            CalleeFile::Unaddressed => None,
        };
        match declaration {
            Some(declaration) => mapped.callees.push(declaration),
            None => mapped.dropped += 1,
        }
    }
    Ok(mapped)
}

async fn resolve_symbol_references(
    reads: &ReadService,
    engines: &EnginePool,
    identity: &CoreSymbolId,
    deadline: Instant,
) -> Result<SymbolReferences, ReadError> {
    let Some((slot, file, symbol)) = reference_source(reads, engines, identity) else {
        return Ok(SymbolReferences::NotServed);
    };
    let language = file.syntax().language().clone();
    let target = ReferenceTarget::new(slot.workspace_root(), reads.index().root(), file, symbol)?;
    let answered = Box::pin(references_on_engine(slot, &target, deadline)).await;
    let (report, unconfirmed) = match answered {
        Ok((report, unconfirmed)) => (report, unconfirmed.then(|| slot.name().to_owned())),
        Err(error) if matches!(error.fault(), EngineFault::CapabilityAbsent { .. }) => {
            return Ok(SymbolReferences::NotServed);
        }
        Err(error) => match error.fault() {
            EngineFault::Analyzing { attempts } => {
                return Ok(SymbolReferences::Unsettled {
                    language,
                    attempts: *attempts,
                });
            }
            _ => return Err(ReadFault::engine(error)),
        },
    };
    symbol_references(
        map_references(reads, identity, report, slot.workspace_root()),
        language,
        unconfirmed,
    )
}

/// What one mapped engine answer contributes to the read.
///
/// An answer the served bytes cannot carry drops the engine's contribution and
/// leaves the indexed relationships standing. Every other failure is the engine
/// tier's own, and still refuses the read: a broken document URI or a workspace
/// root the server cannot address says nothing about the engine's revision, and
/// degrading it would hide a defect behind a warning.
fn symbol_references(
    mapped: Result<Vec<GraphHop>, ReadError>,
    language: Language,
    unconfirmed: Option<String>,
) -> Result<SymbolReferences, ReadError> {
    match mapped {
        Ok(edges) => Ok(SymbolReferences::Resolved {
            edges,
            language,
            unconfirmed,
        }),
        Err(error) if matches!(error.fault(), ReadFault::EngineAnswer { .. }) => {
            Ok(SymbolReferences::Unmapped {
                language,
                detail: error.detail(),
            })
        }
        Err(error) => Err(error),
    }
}

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
    ) -> SessionFuture<'session, Result<(), EngineError>>
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
                    tracing::warn!(
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
    ) -> Result<Self, ReadError> {
        let source = file.source();
        let offset = declaration_name_offset(file, symbol)?;
        let index = LineIndex::new(source);
        let position = |encoding| {
            index
                .position(encoding, offset)
                .map_err(|error| ReadFault::task("reference position conversion", error.detail()))
        };
        Ok(Self {
            path: file.path().clone(),
            language: file.syntax().language().name.clone(),
            uri: TreeRoot::new(workspace_root)
                .and_then(|root| root.document_uri(file.path()))
                .map_err(|error| ReadFault::task("reference URI conversion", error.detail()))?,
            canonical_uri: TreeRoot::new(canonical_root)
                .and_then(|root| root.document_uri(file.path()))
                .map_err(|error| ReadFault::task("reference URI conversion", error.detail()))?,
            source: source.to_owned(),
            utf8: position(PositionEncoding::Utf8)?,
            utf16: position(PositionEncoding::Utf16)?,
        })
    }
}

#[derive(PartialEq)]
struct ReferenceReport {
    locations: Result<Vec<Location>, usize>,
    encoding: PositionEncoding,
    full: bool,
}

async fn references_on_engine(
    slot: &EngineSlot,
    target: &ReferenceTarget,
    deadline: Instant,
) -> Result<(ReferenceReport, bool), EngineError> {
    let request_path = target.path.clone();
    let utf8 = target.utf8;
    let utf16 = target.utf16;
    let uri = target.uri.clone();
    let canonical_uri = target.canonical_uri.clone();
    slot.request_settled(
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
                let mut locations = session.references(&path, position).await?;
                if locations.len() > TRAVERSAL_NODES_MAX {
                    return Ok(ReferenceReport {
                        locations: Err(locations.len()),
                        encoding,
                        full: true,
                    });
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
                let full = locations.iter().any(|location| {
                    (location.uri == uri || location.uri == canonical_uri)
                        && location.range.start <= position
                        && position < location.range.end
                });
                Ok(ReferenceReport {
                    locations: Ok(locations),
                    encoding,
                    full,
                })
            })
        },
        target.closing(),
        |report| {
            (
                report.full,
                report
                    .locations
                    .as_ref()
                    .is_ok_and(|locations| locations.len() <= 1),
            )
        },
        deadline,
    )
    .await
}

fn map_references(
    reads: &ReadService,
    target: &CoreSymbolId,
    report: ReferenceReport,
    workspace_root: &std::path::Path,
) -> Result<Vec<GraphHop>, ReadError> {
    let locations = report.locations.map_err(|observed| ReadFault::engine_answer(
        "engine references", format!("reference locations {observed} exceed the {TRAVERSAL_NODES_MAX}-node traversal bound")))?;
    let root = TreeRoot::new(workspace_root)
        .map_err(|error| ReadFault::task("reference root conversion", error.detail()))?;
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
            relationship: Relationship {
                from: caller,
                to: SymbolId(target.as_str().to_owned()),
                kind: ExactKind(rift_core::fault_label(&RelationshipFacet::References)),
                facets: vec![RelationshipFacet::References],
                evidence: Vec::new(),
                derivation: RelationshipDerivation::Resolution,
                confidence: None,
                extensions: Extensions::default(),
            },
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
) -> Result<Option<SymbolId>, ReadError> {
    let relative = root.project_path(at.uri).or_else(|error| {
        if matches!(error.fault(), UriFault::OutsideRoot) {
            TreeRoot::new(reads.index().root())?.project_path(at.uri)
        } else {
            Err(error)
        }
    });
    let path = match relative {
        Ok(path) => path,
        Err(error) if matches!(error.fault(), UriFault::OutsideRoot) => return Ok(None),
        Err(error) => return Err(ReadFault::task("reference URI conversion", error.detail())),
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
) -> Result<Option<SymbolId>, ReadError> {
    let Some(file) = reads.index().file(path) else {
        return Ok(None);
    };
    let index = LineIndex::new(file.source());
    let convert = |position| {
        index
            .byte_offset(encoding, position)
            .map_err(|error| ReadFault::engine_answer(operation, error.detail()))
    };
    let start = convert(range.start)? as u64;
    let end = convert(range.end)? as u64;
    if start > end {
        return Err(ReadFault::engine_answer(
            operation,
            "an engine range ends before it starts",
        ));
    }
    Ok(file
        .enclosing_symbol(start, end)
        .map(|symbol| symbol_id(file, symbol)))
}

/// Reads the exact name range retained by the syntax provider.
fn declaration_name_offset(file: &IndexedFile, symbol: &SyntaxSymbol) -> Result<usize, ReadError> {
    let range = symbol
        .name_range
        .ok_or_else(|| ReadFault::unsupported("declaration name range"))?;
    let invalid = || {
        ReadFault::task(
            "reference position conversion",
            "declaration name range exceeds indexed source",
        )
    };
    let start = usize::try_from(range.start).map_err(|_| invalid())?;
    let end = usize::try_from(range.end).map_err(|_| invalid())?;
    if file.source().get(start..end).is_none() {
        return Err(invalid());
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
    use rift_index::WorkspaceIndexLimits;
    use rift_lsp::capabilities::PositionEncoding;
    use rift_lsp::uri::TreeRoot;
    use rift_protocol::configuration::{HistoryConfiguration, LspConfiguration};
    use rift_protocol::read::{ExactKind, GetSymbolParams, SearchParams, SymbolId};
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
            ["max", "max~1", "max", "join", "seed", "max~1"].map(id),
        );
        let reached: Vec<_> = hops
            .iter()
            .map(|hop| hop.relationship.to.0.rsplit('/').next().unwrap_or_default())
            .collect();
        assert_eq!(reached, ["join", "max", "max~1"]);
        for hop in &hops {
            assert_eq!(hop.direction, rift_protocol::read::HopDirection::Outgoing);
            assert_eq!(hop.relationship.from, id("seed"));
            assert_eq!(
                hop.relationship.facets,
                [rift_protocol::read::RelationshipFacet::Calls]
            );
        }
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
    fn item(location: Location, name: &str) -> lsp_types::CallHierarchyItem {
        lsp_types::CallHierarchyItem {
            name: name.to_owned(),
            kind: lsp_types::SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: location.uri,
            range: location.range,
            selection_range: location.range,
            data: None,
        }
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
        fs::write(
            directory.path().join("lib.rs"),
            "pub fn beacon() {}\n\npub fn larger() { beacon(); }\n",
        )?;
        let reads = reads(directory.path())?;
        let beacon = location(directory.path(), "lib.rs", 0, 7, 13);
        let stub = Location {
            uri: "vendored://stdlib/json/__init__.pyi".parse()?,
            range: beacon.range,
        };
        let package = rift_core::PackageIdentity {
            manager: "cargo".to_owned(),
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
        let mapped = map_callees(&reads, report, &caller, (&roots, &[directory.path()]))?;
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

    /// A callee range the served bytes do not hold, and an answer past the node bound, are
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
            Err(super::TRAVERSAL_NODES_MAX + 1),
        ] {
            let report = CalleeReport {
                calls,
                encoding: PositionEncoding::Utf16,
            };
            let roots = CalleeRoots::default();
            let Err(error) = map_callees(&reads, report, &caller, (&roots, &[directory.path()]))
            else {
                panic!("an unmappable answer is refused");
            };
            assert!(
                matches!(error.fault(), super::ReadFault::EngineAnswer { .. }),
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
        let len = SymbolId("rift://symbol/python/stdlib/python@3.12.9/builtins.pyi/len".into());
        let python = rift_core::PackageIdentity {
            manager: "stdlib".to_owned(),
            name: "python".to_owned(),
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
                package: python.clone(),
            })
        });
        let edges: Vec<&SymbolId> = references
            .outgoing(&caller)
            .iter()
            .map(|edge| &edge.relationship.to)
            .collect();
        assert_eq!(edges, [&len], "one edge per callee declaration");
        assert_eq!(references.dropped_callees, 1, "`loads` is named nothing");
        assert!(references.package_callees().is_empty());
        let declaration = references
            .package_declaration(&rift_core::SymbolId::new(len.0.clone())?)
            .ok_or("the edge's end is a package declaration")?;
        assert_eq!(
            declaration.unit.0,
            "rift://source/stdlib/python@3.12.9/builtins.pyi"
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

    /// An outgoing walk keeps the refusals an incoming walk draws: beside `rev`, beside
    /// `change`, and with `scope: "global"`.
    #[test]
    fn an_outgoing_walk_keeps_the_incoming_walk_refusals() {
        let seed = "rift://symbol/rust/lib.rs/beacon";
        for (extra, code) in [
            (json!({"rev": "main"}), "capability_unavailable"),
            (
                json!({"change": {"base": "baseline"}}),
                "capability_unavailable",
            ),
            (json!({"scope": "global"}), "invalid_request"),
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
                assert_eq!(refused.descriptor().code(), code, "{request}: {refused}");
            }
        }
    }

    fn reads(root: &std::path::Path) -> Result<ReadService, crate::ReadError> {
        ReadService::build(
            root,
            WorkspaceIndexLimits::default(),
            &SourceVisibility::default(),
            &TextFileInclusion::default(),
            HistoryConfiguration::default(),
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
        serde_json::from_value(json!({"target":"symbol","traversal":{"seed":seed,"direction":"incoming","facets":["references"]}})).expect("traversal request")
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
    fn reference_mapping_retains_cross_file_callers_and_deduplicates() -> TestResult {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("lib.rs"), "pub fn beacon() {}\n")?;
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
            locations: Ok(vec![own, caller.clone(), other, caller, canonical]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let edges = map_references(&reads, &target, report, directory.path())?;
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].relationship.from, symbol(&reads, "caller"));
        assert_eq!(edges[0].relationship.to, seed);
        assert_eq!(
            edges[0].relationship.facets,
            vec![rift_protocol::read::RelationshipFacet::References]
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
                locations: Ok(vec![location(directory.path(), "lib.rs", 0, start, end)]),
                encoding: PositionEncoding::Utf16,
                full: true,
            };
            assert!(map_references(&reads, &seed, report, directory.path()).is_err());
        }
        let overflow = ReferenceReport {
            locations: Err(super::TRAVERSAL_NODES_MAX + 1),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        assert!(map_references(&reads, &seed, overflow, directory.path()).is_err());
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
            locations: Ok(vec![
                location(outside.path(), "lib.rs", 0, 0, 1),
                location(directory.path(), "absent.rs", 0, 0, 1),
            ]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        assert!(map_references(&reads, &target, report, directory.path())?.is_empty());
        let mut refused = location(directory.path(), "lib.rs", 0, 0, 1);
        refused.uri = "https://example.com/lib.rs".parse()?;
        let report = ReferenceReport {
            locations: Ok(vec![refused]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let error =
            map_references(&reads, &target, report, directory.path()).expect_err("scheme refused");
        assert!(error.detail().contains("scheme_refused"), "{error}");
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
            Err(super::ReadFault::engine_answer(
                "engine references",
                "character out of range",
            )),
            language.clone(),
            None,
        );
        assert!(matches!(
            unmapped,
            Ok(super::SymbolReferences::Unmapped { detail, .. })
                if detail.contains("character out of range")
        ));
        let refused = super::symbol_references(
            Err(super::ReadFault::task(
                "reference URI conversion",
                "scheme refused",
            )),
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
        let target = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
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
        let absent = rift_core::SymbolId::new("rift://symbol/toml/absent.toml/beacon")?;
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
        assert_eq!(
            references.incoming(&seed)[0].relationship.from.0,
            caller.as_str()
        );
        assert_eq!(
            references.incoming(&caller)[0].relationship.from,
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
        let count = super::TRAVERSAL_NODES_MAX + 1;
        let locations = vec![location(directory.path(), "lib.rs", 0, 7, 13); count];
        let responses = (1..=2)
            .map(|id| {
                let body =
                    serde_json::to_string(&json!({"jsonrpc":"2.0","id":id,"result":locations}))?;
                Ok(format!("Content-Length: {}\r\n\r\n{body}", body.len()))
            })
            .collect::<Result<Vec<_>, serde_json::Error>>()?;
        let mut fixture = super::process_lifecycle::retrying(
            super::process_lifecycle::answers(&responses, &["rust"]),
            2,
        );
        let rift_protocol::configuration::CommandInput::ProgramAndArguments(command) = fixture
            .configuration
            .command
            .as_mut()
            .expect("process command")
        else {
            panic!("fixture uses arguments");
        };
        let gate = directory.path().join("finish.gate");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&gate)
                .status()?
                .success()
        );
        let mut release = fs::OpenOptions::new().read(true).write(true).open(&gate)?;
        let script = directory.path().join("responses.sh");
        let waiting = command[2].replace("sleep 0.2", &format!("read -r _ < '{}'", gate.display()));
        fs::write(&script, waiting)?;
        command.truncate(1);
        command.push(script.to_string_lossy().into_owned());
        let engines = pool(directory.path(), "rust", fixture.configuration);
        let result = Box::pin(resolve_engine_references(
            &reads,
            &engines,
            &request(&symbol(&reads, "beacon")),
            walk(),
        ))
        .await;
        std::io::Write::write_all(&mut release, b"done\n")?;
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
        let reads = reads(directory.path())?;
        let target = rift_core::SymbolId::new(symbol(&reads, "beacon").0)?;
        let prior = rift_core::SymbolId::new(symbol(&reads, "caller").0)?;
        let report = ReferenceReport {
            locations: Ok(vec![location(directory.path(), "main.py", 4, 11, 17)]),
            encoding: PositionEncoding::Utf16,
            full: true,
        };
        let edge = map_references(&reads, &target, report, directory.path())?.remove(0);
        let prior_edges = (0..super::TRAVERSAL_NODES_MAX)
            .map(|index| {
                let mut edge = edge.clone();
                edge.relationship.from =
                    SymbolId(format!("rift://symbol/python/prior.py/caller_{index}"));
                edge.relationship.to = SymbolId(prior.as_str().to_owned());
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
        assert!(
            reads
                .search_with_references(
                    &request(&symbol(&reads, "beacon")),
                    &StoreAnswer::identifier_only(),
                    &references
                )
                .is_err()
        );
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
        assert_eq!(refused.descriptor().code(), "capability_unavailable");
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
        assert!(matches!(error.fault(), crate::ReadFault::Engine(_)));
        assert!(error.detail().contains("absolute"), "{}", error.detail());
        Ok(())
    }

    #[tokio::test]
    async fn embedded_engine_resolves_cross_file_reference_without_writes() -> TestResult {
        let directory = tempfile::tempdir()?;
        let declaration = "def beacon() -> int:\n    return 7\n";
        let caller = "from helper import beacon\n\ndef caller() -> int:\n    return beacon()\n";
        fs::write(directory.path().join("helper.py"), declaration)?;
        fs::write(directory.path().join("main.py"), caller)?;
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
        assert_eq!(refused.descriptor().code(), "capability_unavailable");
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
        assert!(matches!(error.fault(), crate::ReadFault::Engine(_)));
        assert!(
            error.detail().contains("references refused"),
            "{}",
            error.detail()
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
            json!({"limit":0}),
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
            assert_eq!(error.descriptor().code(), expected.descriptor().code());
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
        calls.traversal.as_mut().expect("traversal").facets =
            vec![rift_protocol::read::RelationshipFacet::Calls];
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
        let seed = rift_core::SymbolId::new("rift://symbol/rust/lib.rs/leaf")?;
        let caller = rift_core::SymbolId::new("rift://symbol/rust/lib.rs/branch_a")?;
        let mut traversal = request(&SymbolId(seed.as_str().to_owned()))
            .traversal
            .expect("traversal");
        traversal.facets.clear();
        let selected = |identity: &rift_core::SymbolId| identity == &caller;
        assert!(!selected(&seed));
        assert!(!super::reachable_reference_source(
            &store, &seed, &traversal, selected
        ));
        traversal.depth = 2;
        assert!(super::reachable_reference_source(
            &store, &seed, &traversal, selected
        ));
        traversal.facets = vec![rift_protocol::read::RelationshipFacet::References];
        assert!(!super::reachable_reference_source(
            &store, &seed, &traversal, selected
        ));
        Ok(())
    }
}
