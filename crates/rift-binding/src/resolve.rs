//! Bounded work-queue resolution of every reference to the definitions it can name.
//!
//! One reference seeds one work item at its anchor scope. Each popped item consults the
//! definitions its next segment names, the imports at its scope, and, in lexical mode, the
//! scope's parent. Complete paths compete by rank, and the survivors' definitions are the
//! targets. Every enqueue is charged to the reference and publication work bounds, so the
//! queue never holds more than `reference_work_max` items and draining it costs the same.

use std::collections::{BTreeSet, VecDeque};

use rift_core::LoopBudget;

use crate::failure::{BindingError, BindingViolation, binding_error};
use crate::graph::{
    DefinitionId, DefinitionOrder, Link, LinkId, LinkKind, NAME_PATH_SEGMENTS_MAX, Name, NamePath,
    PathAnchor, Rank, ReferenceId, ScopeId, ScopeKind,
};
use crate::limits::{BindingLimits, ExhaustedLimit};
use crate::link::LinkedGraph;

/// Rank of a definition found in the current scope.
pub const DEFINITION_RANK: Rank = Rank::new(0);
/// Rank of a step to the lexical parent scope.
pub const LEXICAL_RANK: Rank = Rank::new(2);
/// Work items between two cancellation checks.
pub const CANCELLATION_CHECK_INTERVAL: usize = 64;

/// A cancellation signal the resolver polls every `CANCELLATION_CHECK_INTERVAL` work items.
pub trait Cancellation: Send + Sync {
    /// Whether the caller wants resolution to stop.
    fn is_cancelled(&self) -> bool;
}

/// A signal that never cancels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NeverCancelled;

impl Cancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

/// How a work item looks names up at its scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LookupMode {
    /// The scope and its lexical parents, without visibility filtering.
    Lexical,
    /// The scope alone, filtered by visibility from the item's viewpoint.
    Member,
}

/// Whether definitions open their member scopes during a lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemberScopes {
    /// Member scopes are followed.
    Follow,
    /// Only declared scopes are followed.
    Ignore,
}

/// Facts shared by every work item of one lookup.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LookupContext {
    /// Byte the reference starts at; sequential definitions after it stay hidden.
    pub(crate) position: u64,
    /// Whether member scopes are followed.
    pub(crate) members: MemberScopes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Definition(DefinitionId),
    Link(LinkId),
    Lexical(ScopeId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PathStep {
    step: Step,
    rank: Rank,
}

type VisitKey = (ScopeId, ScopeId, LookupMode, Name, Vec<Name>);

/// One pending lookup: the next segment `head`, what follows it, and how it got here.
#[derive(Debug, Clone)]
struct WorkItem {
    scope: ScopeId,
    viewpoint: ScopeId,
    mode: LookupMode,
    head: Name,
    rest: Vec<Name>,
    steps: Vec<PathStep>,
}

impl WorkItem {
    fn key(&self) -> VisitKey {
        (
            self.scope,
            self.viewpoint,
            self.mode,
            self.head.clone(),
            self.rest.clone(),
        )
    }

    fn extended(&self, step: PathStep) -> Vec<PathStep> {
        let mut steps = self.steps.clone();
        steps.push(step);
        steps
    }
}

struct CompletePath {
    steps: Vec<PathStep>,
    target: DefinitionId,
}

enum Stop {
    Exhausted(ExhaustedLimit),
    Cancelled,
}

struct Search {
    context: LookupContext,
    queue: VecDeque<WorkItem>,
    visited: BTreeSet<VisitKey>,
    complete: Vec<CompletePath>,
    budget: LoopBudget,
    work: u64,
}

impl Search {
    fn new(context: LookupContext, work_max: usize) -> Self {
        Self {
            context,
            queue: VecDeque::new(),
            visited: BTreeSet::new(),
            complete: Vec::new(),
            budget: LoopBudget::new(work_max),
            work: 0,
        }
    }

    fn record_complete(
        &mut self,
        steps: Vec<PathStep>,
        target: DefinitionId,
        targets_max: usize,
    ) -> Result<(), Stop> {
        if self.complete.len() >= targets_max {
            return Err(Stop::Exhausted(ExhaustedLimit::ReferenceTargets));
        }
        self.complete.push(CompletePath { steps, target });
        Ok(())
    }
}

/// Everything resolution established for one reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    targets: Vec<DefinitionId>,
    exhausted: Option<ExhaustedLimit>,
    work: u64,
}

impl Resolution {
    const fn unresolved() -> Self {
        Self {
            targets: Vec::new(),
            exhausted: None,
            work: 0,
        }
    }

    const fn resolved(targets: Vec<DefinitionId>, work: u64) -> Self {
        Self {
            targets,
            exhausted: None,
            work,
        }
    }

    const fn stopped(limit: ExhaustedLimit, work: u64) -> Self {
        Self {
            targets: Vec::new(),
            exhausted: Some(limit),
            work,
        }
    }

    /// Definitions the reference names, sorted by id; empty when unresolved or exhausted.
    #[must_use]
    pub fn targets(&self) -> &[DefinitionId] {
        &self.targets
    }

    /// The bound that stopped this reference, where one did.
    #[must_use]
    pub const fn exhausted(&self) -> Option<ExhaustedLimit> {
        self.exhausted
    }

    /// Work items enqueued for this reference.
    #[must_use]
    pub const fn work(&self) -> u64 {
        self.work
    }
}

/// One [`Resolution`] per reference, in reference order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionSet {
    by_reference: Vec<Resolution>,
    work_total: u64,
    exhausted: Option<ExhaustedLimit>,
}

impl ResolutionSet {
    /// Returns every resolution, in reference order.
    #[must_use]
    pub fn resolutions(&self) -> &[Resolution] {
        &self.by_reference
    }

    /// Returns the resolution of one reference.
    #[must_use]
    pub fn resolution(&self, reference: ReferenceId) -> &Resolution {
        &self.by_reference[reference.index()]
    }

    /// Work items enqueued across every reference.
    #[must_use]
    pub const fn work_total(&self) -> u64 {
        self.work_total
    }

    /// The publication bound that stopped resolution, where one did.
    #[must_use]
    pub const fn exhausted(&self) -> Option<ExhaustedLimit> {
        self.exhausted
    }
}

/// Resolves every reference of a linked graph.
///
/// A reference whose anchor names no scope is unresolved. Once `publication_work_max` is
/// spent, every later reference is stopped with that limit and the set carries it.
///
/// # Errors
///
/// Returns [`BindingViolation::Cancelled`] when `cancellation` reports cancellation at a
/// check.
pub fn resolve_all(
    linked: &LinkedGraph<'_>,
    limits: &BindingLimits,
    cancellation: &dyn Cancellation,
) -> Result<ResolutionSet, BindingError> {
    let mut resolver = Resolver::new(linked, limits, cancellation);
    let graph = linked.graph();
    let mut by_reference = Vec::with_capacity(graph.references().len());
    let mut exhausted = None;
    for reference in graph.reference_ids() {
        if exhausted.is_some() {
            by_reference.push(Resolution::stopped(ExhaustedLimit::PublicationWork, 0));
            continue;
        }
        let resolution = resolver.resolve_reference(reference)?;
        if resolution.exhausted() == Some(ExhaustedLimit::PublicationWork) {
            exhausted = resolution.exhausted();
        }
        by_reference.push(resolution);
    }
    Ok(ResolutionSet {
        by_reference,
        work_total: resolver.work_total,
        exhausted,
    })
}

/// Drives lookups over one linked graph under one publication work budget.
pub(crate) struct Resolver<'a> {
    linked: &'a LinkedGraph<'a>,
    limits: &'a BindingLimits,
    cancellation: &'a dyn Cancellation,
    publication_budget: LoopBudget,
    work_total: u64,
    until_cancellation_check: usize,
}

impl<'a> Resolver<'a> {
    pub(crate) fn new(
        linked: &'a LinkedGraph<'a>,
        limits: &'a BindingLimits,
        cancellation: &'a dyn Cancellation,
    ) -> Self {
        Self {
            linked,
            limits,
            cancellation,
            publication_budget: LoopBudget::new(limits.publication_work_max()),
            work_total: 0,
            until_cancellation_check: CANCELLATION_CHECK_INTERVAL,
        }
    }

    fn resolve_reference(&mut self, id: ReferenceId) -> Result<Resolution, BindingError> {
        let reference = self.linked.graph().reference(id);
        let Some(start) = self
            .linked
            .anchor_scope(reference.scope(), reference.anchor())
        else {
            return Ok(Resolution::unresolved());
        };
        let context = LookupContext {
            position: reference.range().start(),
            members: MemberScopes::Follow,
        };
        self.resolve_path(
            start,
            reference.anchor(),
            reference.scope(),
            reference.path(),
            context,
        )
    }

    /// Resolves `path` from `start`, looking its first segment up the way `anchor` says
    /// and judging visibility from `viewpoint`.
    pub(crate) fn resolve_path(
        &mut self,
        start: ScopeId,
        anchor: PathAnchor,
        viewpoint: ScopeId,
        path: &NamePath,
        context: LookupContext,
    ) -> Result<Resolution, BindingError> {
        let seed = WorkItem {
            scope: start,
            viewpoint,
            mode: seed_mode(anchor),
            head: path.head().clone(),
            rest: path.tail().to_vec(),
            steps: Vec::new(),
        };
        let mut search = Search::new(context, self.limits.reference_work_max());
        let outcome = self.run(&mut search, seed);
        self.work_total += search.work;
        match outcome {
            Ok(()) => Ok(Resolution::resolved(
                surviving_targets(&search.complete),
                search.work,
            )),
            Err(Stop::Exhausted(limit)) => Ok(Resolution::stopped(limit, search.work)),
            Err(Stop::Cancelled) => Err(binding_error(
                BindingViolation::Cancelled,
                format!("after {} work items", self.work_total),
            )),
        }
    }

    /// Drains the queue; every popped item was enqueued under the reference work budget, so
    /// the loop runs at most `reference_work_max` times.
    fn run(&mut self, search: &mut Search, seed: WorkItem) -> Result<(), Stop> {
        self.enqueue(search, seed)?;
        while let Some(item) = search.queue.pop_front() {
            self.expand(search, &item)?;
        }
        Ok(())
    }

    fn enqueue(&mut self, search: &mut Search, item: WorkItem) -> Result<(), Stop> {
        let too_deep = item.steps.len() > self.limits.path_depth_max()
            || item.rest.len() >= NAME_PATH_SEGMENTS_MAX;
        if too_deep {
            return Err(Stop::Exhausted(ExhaustedLimit::PathDepth));
        }
        if !search.visited.insert(item.key()) {
            return Ok(());
        }
        search
            .budget
            .consume()
            .map_err(|_| Stop::Exhausted(ExhaustedLimit::ReferenceWork))?;
        self.publication_budget
            .consume()
            .map_err(|_| Stop::Exhausted(ExhaustedLimit::PublicationWork))?;
        search.work += 1;
        self.check_cancellation()?;
        search.queue.push_back(item);
        Ok(())
    }

    fn check_cancellation(&mut self) -> Result<(), Stop> {
        self.until_cancellation_check -= 1;
        if self.until_cancellation_check > 0 {
            return Ok(());
        }
        self.until_cancellation_check = CANCELLATION_CHECK_INTERVAL;
        if self.cancellation.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        Ok(())
    }

    fn expand(&mut self, search: &mut Search, item: &WorkItem) -> Result<(), Stop> {
        self.expand_definitions(search, item)?;
        self.expand_imports(search, item)?;
        self.expand_lexical(search, item)
    }

    fn expand_definitions(&mut self, search: &mut Search, item: &WorkItem) -> Result<(), Stop> {
        let targets_max = self.limits.reference_targets_max();
        for definition in self.matching_definitions(item, search.context) {
            let step = PathStep {
                step: Step::Definition(definition),
                rank: DEFINITION_RANK,
            };
            let steps = item.extended(step);
            match item.rest.split_first() {
                None => search.record_complete(steps, definition, targets_max)?,
                Some(pending) => self.enqueue_opened(search, item, definition, pending, &steps)?,
            }
        }
        Ok(())
    }

    fn enqueue_opened(
        &mut self,
        search: &mut Search,
        item: &WorkItem,
        definition: DefinitionId,
        (head, rest): (&Name, &[Name]),
        steps: &[PathStep],
    ) -> Result<(), Stop> {
        let linked = self.linked;
        for scope in linked.opened_scopes(definition, search.context.members) {
            let next = WorkItem {
                scope,
                viewpoint: item.viewpoint,
                mode: LookupMode::Member,
                head: head.clone(),
                rest: rest.to_vec(),
                steps: steps.to_vec(),
            };
            self.enqueue(search, next)?;
        }
        Ok(())
    }

    /// Definitions named by `item.head` at its scope: the latest sequential definition
    /// before the reference, else every item definition; member mode also filters by
    /// visibility from the item's viewpoint.
    fn matching_definitions(&self, item: &WorkItem, context: LookupContext) -> Vec<DefinitionId> {
        let graph = self.linked.graph();
        let mut items = Vec::new();
        let mut latest: Option<(u64, DefinitionId)> = None;
        for &definition in graph.definitions_named(item.scope, &item.head) {
            if item.mode == LookupMode::Member && !self.linked.visible(definition, item.viewpoint) {
                continue;
            }
            match graph.definition(definition).order() {
                DefinitionOrder::Item => items.push(definition),
                DefinitionOrder::Sequential(position) if position <= context.position => {
                    latest = Some(later_of(latest, (position, definition)));
                }
                DefinitionOrder::Sequential(_) => {}
            }
        }
        match latest {
            Some((_, definition)) => vec![definition],
            None => items,
        }
    }

    fn expand_imports(&mut self, search: &mut Search, item: &WorkItem) -> Result<(), Stop> {
        let linked = self.linked;
        for &link_id in linked.graph().links_at(item.scope) {
            let Some(next) = self.import_item(item, link_id, linked.graph().link(link_id)) else {
                continue;
            };
            self.enqueue(search, next)?;
        }
        Ok(())
    }

    /// The item an import at the current scope continues with, when it binds `item.head`.
    ///
    /// An import contributes at most one step per path: a link already in `item.steps` is
    /// not followed again, so a wildcard rewriting its own scope's lookups cannot grow a
    /// path without bound.
    fn import_item(&self, item: &WorkItem, link_id: LinkId, link: &Link) -> Option<WorkItem> {
        let LinkKind::Import {
            name, anchor, path, ..
        } = link.kind()
        else {
            return None;
        };
        let named = name.as_ref().is_none_or(|alias| *alias == item.head);
        let fresh = !item
            .steps
            .iter()
            .any(|prior| prior.step == Step::Link(link_id));
        let visible =
            item.mode == LookupMode::Lexical || self.linked.link_visible(link_id, item.viewpoint);
        if !named || !fresh || !visible {
            return None;
        }
        let scope = self.linked.anchor_scope(link.scope(), *anchor)?;
        let wildcard = name.is_none().then_some(&item.head);
        let rest = path
            .tail()
            .iter()
            .chain(wildcard)
            .chain(&item.rest)
            .cloned()
            .collect();
        let step = PathStep {
            step: Step::Link(link_id),
            rank: link.rank(),
        };
        Some(WorkItem {
            scope,
            viewpoint: link.scope(),
            mode: seed_mode(*anchor),
            head: path.head().clone(),
            rest,
            steps: item.extended(step),
        })
    }

    /// Steps to the lexical parent; module scopes do not inherit their parent's names.
    fn expand_lexical(&mut self, search: &mut Search, item: &WorkItem) -> Result<(), Stop> {
        let scope = self.linked.graph().scope(item.scope);
        let lookup = (item.mode, scope.kind(), scope.parent());
        let (LookupMode::Lexical, ScopeKind::Block | ScopeKind::Member, Some(parent)) = lookup
        else {
            return Ok(());
        };
        let step = PathStep {
            step: Step::Lexical(parent),
            rank: LEXICAL_RANK,
        };
        let next = WorkItem {
            scope: parent,
            viewpoint: item.viewpoint,
            mode: LookupMode::Lexical,
            head: item.head.clone(),
            rest: item.rest.clone(),
            steps: item.extended(step),
        };
        self.enqueue(search, next)
    }
}

const fn seed_mode(anchor: PathAnchor) -> LookupMode {
    match anchor {
        PathAnchor::Lexical => LookupMode::Lexical,
        PathAnchor::Crate | PathAnchor::SelfModule | PathAnchor::Super(_) => LookupMode::Member,
    }
}

/// Keeps the later position; on a tie the earlier definition stays.
fn later_of(
    current: Option<(u64, DefinitionId)>,
    candidate: (u64, DefinitionId),
) -> (u64, DefinitionId) {
    match current {
        Some(existing) if existing.0 >= candidate.0 => existing,
        _ => candidate,
    }
}

/// Definitions of the complete paths no other path shadows, sorted and deduplicated.
/// Bounded by `reference_targets_max` squared comparisons of at most `path_depth_max` steps.
fn surviving_targets(complete: &[CompletePath]) -> Vec<DefinitionId> {
    let mut targets: Vec<DefinitionId> = complete
        .iter()
        .filter(|path| !shadowed(&path.steps, complete))
        .map(|path| path.target)
        .collect();
    targets.sort_unstable();
    targets.dedup();
    targets
}

/// Whether some other path shares a prefix with `steps` and outranks it where they first
/// differ.
fn shadowed(steps: &[PathStep], complete: &[CompletePath]) -> bool {
    complete.iter().any(|other| {
        other
            .steps
            .iter()
            .zip(steps)
            .find(|(winner, loser)| winner.step != loser.step)
            .is_some_and(|(winner, loser)| winner.rank < loser.rank)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{Cancellation, NeverCancelled, Resolution, resolve_all};
    use crate::failure::BindingViolation;
    use crate::fixture::{Fixture, resolve, targets};
    use crate::graph::{
        BindingGraph, DefinitionId, DefinitionOrder, PathAnchor, ReferenceId, ScopeId, ScopeKind,
        UnitId, VisibilitySpelling,
    };
    use crate::limits::{BindingLimits, ExhaustedLimit};
    use crate::link::LinkedGraph;

    const PUBLIC: VisibilitySpelling = VisibilitySpelling::Public;
    const PRIVATE: VisibilitySpelling = VisibilitySpelling::Private;

    #[test]
    fn test_resolve_nearest_lexical_definition_wins_over_parent_block() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let outer = fixture.block(unit, Some(root), 10, 90);
        let inner = fixture.block(unit, Some(outer), 20, 80);
        fixture.item(root, "f", 0, 5, PRIVATE);
        let outer_f = fixture.item(outer, "f", 11, 15, PRIVATE);
        let inner_f = fixture.item(inner, "f", 21, 25, PRIVATE);
        let in_inner = fixture.reference(inner, 50, PathAnchor::Lexical, "f");
        let in_outer = fixture.reference(outer, 85, PathAnchor::Lexical, "f");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, in_inner), vec![inner_f]);
        assert_eq!(targets(&set, in_outer), vec![outer_f]);
        assert_eq!(set.resolution(in_inner).exhausted(), None);
        assert_eq!(set.resolutions().len(), 2);
        assert_eq!(set.exhausted(), None);
        assert_eq!(set.work_total(), 3 + 2);
    }

    #[test]
    fn test_resolve_module_scope_does_not_inherit_parent_module() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let body = fixture.module(unit, Some(root), 30, 90);
        fixture.item(root, "f", 0, 5, PUBLIC);
        fixture.declaring_item(root, "m", 20, 90, PUBLIC, body);
        let inside = fixture.reference(body, 50, PathAnchor::Lexical, "f");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, inside), Vec::new());
        assert_eq!(set.resolution(inside).work(), 1);
    }

    #[test]
    fn test_resolve_sequential_shadowing_by_position() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let block = fixture.block(unit, Some(root), 1, 99);
        let first = fixture.sequential(block, "x", 10);
        let second = fixture.sequential(block, "x", 20);
        let before = fixture.reference(block, 5, PathAnchor::Lexical, "x");
        let between = fixture.reference(block, 15, PathAnchor::Lexical, "x");
        let after = fixture.reference(block, 25, PathAnchor::Lexical, "x");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, before), Vec::new());
        assert_eq!(targets(&set, between), vec![first]);
        assert_eq!(targets(&set, after), vec![second]);
    }

    #[test]
    fn test_resolve_sequential_tie_keeps_earlier_definition() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let first = fixture.sequential(root, "x", 10);
        fixture.sequential(root, "x", 10);
        fixture.item(root, "x", 30, 40, PRIVATE);
        let reference = fixture.reference(root, 50, PathAnchor::Lexical, "x");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, reference), vec![first]);
    }

    #[test]
    fn test_resolve_item_definitions_are_order_independent() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let reference = fixture.reference(root, 5, PathAnchor::Lexical, "f");
        let later = fixture.item(root, "f", 50, 60, PRIVATE);
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, reference), vec![later]);
    }

    #[test]
    fn test_resolve_equal_definitions_stay_ambiguous_in_id_order() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let first = fixture.item(root, "f", 50, 60, PRIVATE);
        let second = fixture.item(root, "f", 70, 80, PRIVATE);
        let reference = fixture.reference(root, 5, PathAnchor::Lexical, "f");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert!(first < second);
        assert_eq!(targets(&set, reference), vec![first, second]);
    }

    /// Root module with inline modules `m` and `n`, each declaring `pub fn f`; `n` also
    /// declares `pub fn g`. Returns the root, `m`'s body, `n`'s body, and the definitions.
    fn two_modules(
        fixture: &mut Fixture,
    ) -> (UnitId, ScopeId, ScopeId, ScopeId, [DefinitionId; 3]) {
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 200);
        let m_body = fixture.module(unit, Some(root), 10, 50);
        let n_body = fixture.module(unit, Some(root), 60, 100);
        fixture.declaring_item(root, "m", 5, 50, PUBLIC, m_body);
        fixture.declaring_item(root, "n", 55, 100, PUBLIC, n_body);
        let m_f = fixture.item(m_body, "f", 11, 20, PUBLIC);
        let n_f = fixture.item(n_body, "f", 61, 70, PUBLIC);
        let n_g = fixture.item(n_body, "g", 71, 80, PUBLIC);
        (unit, root, m_body, n_body, [m_f, n_f, n_g])
    }

    #[test]
    fn test_resolve_explicit_import_shadows_wildcard_import() {
        let mut fixture = Fixture::new();
        let (_, root, _, _, [m_f, _, n_g]) = two_modules(&mut fixture);
        fixture.import(root, Some("f"), PathAnchor::SelfModule, "m::f", 0, PRIVATE);
        fixture.import(root, None, PathAnchor::SelfModule, "n", 1, PRIVATE);
        let f = fixture.reference(root, 150, PathAnchor::Lexical, "f");
        let g = fixture.reference(root, 160, PathAnchor::Lexical, "g");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, f), vec![m_f]);
        assert_eq!(targets(&set, g), vec![n_g]);
    }

    #[test]
    fn test_resolve_definition_in_scope_shadows_wildcard_import() {
        let mut fixture = Fixture::new();
        let (_, root, _, _, _) = two_modules(&mut fixture);
        fixture.import(root, None, PathAnchor::SelfModule, "n", 1, PRIVATE);
        let local = fixture.item(root, "f", 120, 130, PRIVATE);
        let f = fixture.reference(root, 150, PathAnchor::Lexical, "f");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, f), vec![local]);
    }

    #[test]
    fn test_resolve_alias_import_binds_alias_only() {
        let mut fixture = Fixture::new();
        let (_, root, _, _, [m_f, _, _]) = two_modules(&mut fixture);
        fixture.import(
            root,
            Some("renamed"),
            PathAnchor::SelfModule,
            "m::f",
            0,
            PRIVATE,
        );
        let alias = fixture.reference(root, 150, PathAnchor::Lexical, "renamed");
        let original = fixture.reference(root, 160, PathAnchor::Lexical, "f");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, alias), vec![m_f]);
        assert_eq!(targets(&set, original), Vec::new());
    }

    #[test]
    fn test_resolve_duplicate_import_links_share_visited_state() {
        let mut fixture = Fixture::new();
        let (_, root, _, _, [m_f, _, _]) = two_modules(&mut fixture);
        fixture.import(root, Some("f"), PathAnchor::SelfModule, "m::f", 0, PRIVATE);
        fixture.import(root, Some("f"), PathAnchor::SelfModule, "m::f", 0, PRIVATE);
        let f = fixture.reference(root, 150, PathAnchor::Lexical, "f");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, f), vec![m_f]);
        let work = set.resolution(f).work();
        assert_eq!(work, 3, "seed, one import item, one opened scope");
    }

    #[test]
    fn test_resolve_path_through_declared_scope() {
        let mut fixture = Fixture::new();
        let (_, root, m_body, _, [m_f, _, _]) = two_modules(&mut fixture);
        let hidden = fixture.item(m_body, "h", 21, 30, PRIVATE);
        let visible = fixture.reference(root, 150, PathAnchor::Lexical, "m::f");
        let private = fixture.reference(root, 160, PathAnchor::Lexical, "m::h");
        let inside = fixture.reference(m_body, 40, PathAnchor::SelfModule, "h");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, visible), vec![m_f]);
        assert_eq!(targets(&set, private), Vec::new());
        assert_eq!(targets(&set, inside), vec![hidden]);
    }

    #[test]
    fn test_resolve_member_scope_through_member_link() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 200);
        let owner = fixture.item(root, "T", 0, 10, PUBLIC);
        let body = fixture.member(unit, Some(root), 20, 60);
        fixture.member_link(root, PathAnchor::Lexical, "T", body);
        let new = fixture.item(body, "new", 21, 30, PUBLIC);
        let block = fixture.block(unit, Some(root), 100, 150);
        let direct = fixture.reference(root, 170, PathAnchor::Lexical, "T::new");
        let nested = fixture.reference(block, 120, PathAnchor::Lexical, "T::new");
        let missing = fixture.reference(root, 180, PathAnchor::Lexical, "T::absent");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, direct), vec![new]);
        assert_eq!(targets(&set, nested), vec![new]);
        assert_eq!(targets(&set, missing), Vec::new());
        assert_eq!(graph.definition(owner).name().as_str(), "T");
    }

    #[test]
    fn test_resolve_member_link_unresolved_owner_attaches_nothing() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 200);
        let body = fixture.member(unit, Some(root), 20, 60);
        let link = fixture.member_link(root, PathAnchor::Lexical, "Missing", body);
        fixture.item(body, "new", 21, 30, PUBLIC);
        let reference = fixture.reference(root, 170, PathAnchor::Lexical, "Missing::new");
        let graph = fixture.build();
        let limits = BindingLimits::default();
        let linked = LinkedGraph::link(&graph, &limits).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert_eq!(linked.unlinked_members(), &[link]);
        let set = resolve_all(&linked, &limits, &NeverCancelled).ok();
        let resolution = set.as_ref().map(|set| set.resolution(reference).clone());
        assert_eq!(resolution.as_ref().map(Resolution::targets), Some(&[][..]));
    }

    #[test]
    fn test_resolve_private_definition_hidden_across_modules_visible_in_own_block() {
        let mut fixture = Fixture::new();
        let (unit, root, m_body, n_body, _) = two_modules(&mut fixture);
        let hidden = fixture.item(m_body, "h", 21, 30, PRIVATE);
        let block = fixture.block(unit, Some(m_body), 31, 45);
        let lexical = fixture.reference(block, 40, PathAnchor::Lexical, "h");
        let own_module = fixture.reference(block, 41, PathAnchor::SelfModule, "h");
        let sibling = fixture.reference(n_body, 90, PathAnchor::Super(1), "m::h");
        fixture.import(n_body, Some("h"), PathAnchor::Super(1), "m::h", 0, PRIVATE);
        let imported = fixture.reference(n_body, 95, PathAnchor::Lexical, "h");
        let from_root = fixture.reference(root, 150, PathAnchor::Lexical, "m::h");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, lexical), vec![hidden]);
        assert_eq!(targets(&set, own_module), vec![hidden]);
        assert_eq!(targets(&set, sibling), Vec::new());
        assert_eq!(targets(&set, imported), Vec::new());
        assert_eq!(targets(&set, from_root), Vec::new());
    }

    #[test]
    fn test_resolve_public_definition_reachable_from_anywhere() {
        let mut fixture = Fixture::new();
        let (_, root, _, n_body, [m_f, _, _]) = two_modules(&mut fixture);
        let sibling = fixture.reference(n_body, 90, PathAnchor::Super(1), "m::f");
        let from_crate = fixture.reference(n_body, 91, PathAnchor::Crate, "m::f");
        let from_root = fixture.reference(root, 150, PathAnchor::SelfModule, "m::f");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, sibling), vec![m_f]);
        assert_eq!(targets(&set, from_crate), vec![m_f]);
        assert_eq!(targets(&set, from_root), vec![m_f]);
    }

    #[test]
    fn test_resolve_anchors_crate_self_super_and_past_chain() {
        let mut fixture = Fixture::new();
        let (unit, root, m_body, _, _) = two_modules(&mut fixture);
        let root_f = fixture.item(root, "f", 120, 130, PRIVATE);
        let block = fixture.block(unit, Some(m_body), 31, 45);
        let via_crate = fixture.reference(block, 40, PathAnchor::Crate, "f");
        let via_super = fixture.reference(block, 41, PathAnchor::Super(1), "f");
        let via_self = fixture.reference(block, 42, PathAnchor::SelfModule, "f");
        let past_chain = fixture.reference(block, 43, PathAnchor::Super(9), "f");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, via_crate), vec![root_f]);
        assert_eq!(targets(&set, via_super), vec![root_f]);
        let m_f = graph
            .definitions_named(m_body, &crate::fixture::name("f"))
            .to_vec();
        assert_eq!(targets(&set, via_self), m_f);
        assert_eq!(targets(&set, past_chain), Vec::new());
        assert_eq!(set.resolution(past_chain).work(), 0);
        assert_eq!(set.resolution(past_chain).exhausted(), None);
    }

    /// Unit `src/main.rs` declaring `mod a;`, and unit `src/a.rs` with `pub fn run`.
    fn two_units(fixture: &mut Fixture, declared: &str) -> (ScopeId, ScopeId, DefinitionId) {
        let main = fixture.unit("src/main.rs");
        let other = fixture.unit("src/a.rs");
        let root = fixture.module(main, None, 0, 100);
        let other_root = fixture.module(other, None, 0, 100);
        let declaration = fixture.item(root, "a", 0, 6, PRIVATE);
        fixture.module_link(root, declaration, declared);
        let run = fixture.item(other_root, "run", 0, 10, PUBLIC);
        (root, other_root, run)
    }

    #[test]
    fn test_resolve_module_link_joins_units() {
        let mut fixture = Fixture::new();
        let (root, _, run) = two_units(&mut fixture, "src/a.rs");
        let reference = fixture.reference(root, 50, PathAnchor::Lexical, "a::run");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, reference), vec![run]);
    }

    #[test]
    fn test_resolve_unknown_unit_leaves_reference_unresolved() {
        let mut fixture = Fixture::new();
        let (root, _, _) = two_units(&mut fixture, "src/missing.rs");
        let reference = fixture.reference(root, 50, PathAnchor::Lexical, "a::run");
        let graph = fixture.build();
        let limits = BindingLimits::default();
        let linked = LinkedGraph::link(&graph, &limits).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert_eq!(linked.unlinked_modules().len(), 1);
        let set = resolve_all(&linked, &limits, &NeverCancelled).ok();
        let targets = set.map(|set| set.resolution(reference).targets().to_vec());
        assert_eq!(targets, Some(Vec::new()));
    }

    #[test]
    fn test_resolve_same_name_definitions_in_unrelated_units_stay_apart() {
        let mut fixture = Fixture::new();
        for path in ["src/a.rs", "src/b.rs"] {
            let unit = fixture.unit(path);
            let root = fixture.module(unit, None, 0, 100);
            fixture.item(root, "f", 0, 10, PUBLIC);
        }
        let third = fixture.unit("src/c.rs");
        let third_root = fixture.module(third, None, 0, 100);
        let bare = fixture.reference(third_root, 50, PathAnchor::Lexical, "f");
        let qualified = fixture.reference(third_root, 60, PathAnchor::Lexical, "a::f");
        let from_crate = fixture.reference(third_root, 70, PathAnchor::Crate, "f");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, bare), Vec::new());
        assert_eq!(targets(&set, qualified), Vec::new());
        assert_eq!(targets(&set, from_crate), Vec::new());
    }

    #[test]
    fn test_resolve_import_cycle_across_units_terminates_without_repeating_work() {
        let mut fixture = Fixture::new();
        let (root, other_root, _) = two_units(&mut fixture, "src/a.rs");
        fixture.import(root, Some("x"), PathAnchor::SelfModule, "a::x", 0, PUBLIC);
        fixture.import(other_root, Some("x"), PathAnchor::Super(1), "x", 0, PUBLIC);
        let reference = fixture.reference(root, 50, PathAnchor::Lexical, "x");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, reference), Vec::new());
        assert_eq!(set.resolution(reference).exhausted(), None);
        assert_eq!(
            set.resolution(reference).work(),
            4,
            "seed, a::x, x at a, x at root"
        );
    }

    #[test]
    fn test_resolve_reexport_chain_across_units() {
        let mut fixture = Fixture::new();
        let names = ["src/a.rs", "src/b.rs", "src/c.rs"];
        let units: Vec<_> = names.iter().map(|path| fixture.unit(path)).collect();
        let roots: Vec<_> = units
            .iter()
            .map(|unit| fixture.module(*unit, None, 0, 100))
            .collect();
        let declares_b = fixture.item(roots[0], "b", 0, 6, PRIVATE);
        fixture.module_link(roots[0], declares_b, names[1]);
        let declares_c = fixture.item(roots[1], "c", 0, 6, PRIVATE);
        fixture.module_link(roots[1], declares_c, names[2]);
        let f = fixture.item(roots[2], "f", 0, 10, PUBLIC);
        let g = fixture.item(roots[2], "g", 11, 20, PUBLIC);
        fixture.import(
            roots[1],
            Some("f"),
            PathAnchor::SelfModule,
            "c::f",
            0,
            PUBLIC,
        );
        fixture.import(
            roots[1],
            Some("g"),
            PathAnchor::SelfModule,
            "c::g",
            0,
            PRIVATE,
        );
        let reexported = fixture.reference(roots[0], 50, PathAnchor::Lexical, "b::f");
        let private_use = fixture.reference(roots[0], 60, PathAnchor::Lexical, "b::g");
        let inside_b = fixture.reference(roots[1], 70, PathAnchor::Lexical, "g");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(targets(&set, reexported), vec![f]);
        assert_eq!(targets(&set, private_use), Vec::new());
        assert_eq!(targets(&set, inside_b), vec![g]);
    }

    /// A chain of `depth` nested blocks under the root; returns the root and the innermost.
    fn block_chain(fixture: &mut Fixture, depth: u64) -> (ScopeId, ScopeId) {
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 10_000);
        let mut parent = root;
        for level in 1..=depth {
            parent = fixture.block(unit, Some(parent), level, 10_000 - level);
        }
        (root, parent)
    }

    #[test]
    fn test_resolve_reference_work_exhausted_yields_no_targets() {
        let mut fixture = Fixture::new();
        let (root, innermost) = block_chain(&mut fixture, 4);
        fixture.item(innermost, "f", 5, 6, PRIVATE);
        fixture.item(root, "f", 1, 2, PRIVATE);
        let reference = fixture.reference(innermost, 50, PathAnchor::Lexical, "f");
        let graph = fixture.build();
        let limits = BindingLimits::builder().reference_work_max(3).build();
        let set = resolve(&graph, &limits.unwrap_or_default());
        let resolution = set.resolution(reference);
        assert_eq!(resolution.targets(), &[]);
        assert_eq!(resolution.exhausted(), Some(ExhaustedLimit::ReferenceWork));
        assert_eq!(resolution.work(), 3);
        let exact = BindingLimits::builder().reference_work_max(5).build();
        let set = resolve(&graph, &exact.unwrap_or_default());
        assert_eq!(set.resolution(reference).targets().len(), 1);
        assert_eq!(set.resolution(reference).work(), 5);
    }

    #[test]
    fn test_resolve_path_depth_exhausted_on_deep_scope_chain() {
        let mut fixture = Fixture::new();
        let (root, innermost) = block_chain(&mut fixture, 5);
        let top = fixture.item(root, "f", 1, 2, PRIVATE);
        let reference = fixture.reference(innermost, 50, PathAnchor::Lexical, "f");
        let graph = fixture.build();
        let limits = BindingLimits::builder().path_depth_max(2).build();
        let set = resolve(&graph, &limits.unwrap_or_default());
        assert_eq!(set.resolution(reference).targets(), &[]);
        assert_eq!(
            set.resolution(reference).exhausted(),
            Some(ExhaustedLimit::PathDepth)
        );
        let exact = BindingLimits::builder().path_depth_max(5).build();
        let set = resolve(&graph, &exact.unwrap_or_default());
        assert_eq!(set.resolution(reference).targets(), &[top]);
    }

    #[test]
    fn test_resolve_reference_targets_exhausted_never_truncates() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let first = fixture.item(root, "f", 1, 2, PRIVATE);
        let second = fixture.item(root, "f", 3, 4, PRIVATE);
        fixture.item(root, "f", 5, 6, PRIVATE);
        let reference = fixture.reference(root, 50, PathAnchor::Lexical, "f");
        let graph = fixture.build();
        let limits = BindingLimits::builder().reference_targets_max(2).build();
        let set = resolve(&graph, &limits.unwrap_or_default());
        assert_eq!(set.resolution(reference).targets(), &[]);
        let exhausted = set.resolution(reference).exhausted();
        assert_eq!(exhausted, Some(ExhaustedLimit::ReferenceTargets));
        let exact = BindingLimits::builder().reference_targets_max(3).build();
        let set = resolve(&graph, &exact.unwrap_or_default());
        assert_eq!(set.resolution(reference).targets().len(), 3);
        assert_eq!(set.resolution(reference).targets()[..2], [first, second]);
    }

    #[test]
    fn test_resolve_publication_work_exhausted_stops_set() {
        let mut fixture = Fixture::new();
        let (root, block) = block_chain(&mut fixture, 1);
        let f = fixture.item(root, "f", 1, 2, PRIVATE);
        let first = fixture.reference(block, 50, PathAnchor::Lexical, "f");
        let second = fixture.reference(block, 60, PathAnchor::Lexical, "f");
        let third = fixture.reference(block, 70, PathAnchor::Lexical, "f");
        let graph = fixture.build();
        let limits = BindingLimits::builder().publication_work_max(3).build();
        let set = resolve(&graph, &limits.unwrap_or_default());
        assert_eq!(set.exhausted(), Some(ExhaustedLimit::PublicationWork));
        assert_eq!(set.work_total(), 3);
        assert_eq!(set.resolution(first).targets(), &[f]);
        assert_eq!(
            set.resolution(second).exhausted(),
            Some(ExhaustedLimit::PublicationWork)
        );
        assert_eq!(set.resolution(second).work(), 1);
        assert_eq!(
            set.resolution(third).exhausted(),
            Some(ExhaustedLimit::PublicationWork)
        );
        assert_eq!(set.resolution(third).work(), 0);
    }

    #[test]
    fn test_resolve_pending_path_over_segments_max_exhausts_path_depth() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let long = (0..31).map(|_| "a").collect::<Vec<_>>().join("::");
        fixture.import(root, Some("x"), PathAnchor::SelfModule, &long, 0, PRIVATE);
        let reference = fixture.reference(root, 50, PathAnchor::Lexical, "x::y::z");
        let graph = fixture.build();
        let set = resolve(&graph, &BindingLimits::default());
        assert_eq!(
            set.resolution(reference).exhausted(),
            Some(ExhaustedLimit::PathDepth)
        );
    }

    struct CancelAfterChecks {
        checks: AtomicUsize,
        limit: usize,
    }

    impl Cancellation for CancelAfterChecks {
        fn is_cancelled(&self) -> bool {
            self.checks.fetch_add(1, Ordering::SeqCst) + 1 >= self.limit
        }
    }

    #[test]
    fn test_resolve_cancellation_after_checks_yields_cancelled() {
        let mut fixture = Fixture::new();
        let (root, innermost) = block_chain(&mut fixture, 140);
        fixture.item(root, "f", 1, 2, PRIVATE);
        let reference = fixture.reference(innermost, 500, PathAnchor::Lexical, "f");
        let graph = fixture.build();
        let limits = BindingLimits::builder()
            .path_depth_max(200)
            .build()
            .unwrap_or_default();
        let linked = LinkedGraph::link(&graph, &limits).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        let signal = CancelAfterChecks {
            checks: AtomicUsize::new(0),
            limit: 2,
        };
        let cancelled = resolve_all(&linked, &limits, &signal);
        let violation = cancelled
            .as_ref()
            .map_err(|error| error.fault().violation());
        assert_eq!(violation, Err(BindingViolation::Cancelled));
        assert_eq!(signal.checks.load(Ordering::SeqCst), 2);
        let completed = resolve_all(&linked, &limits, &NeverCancelled).ok();
        let targets = completed.map(|set| set.resolution(reference).targets().len());
        assert_eq!(targets, Some(1));
    }

    #[derive(Clone, Copy)]
    struct ScopeSpec {
        label: &'static str,
        unit: &'static str,
        kind: ScopeKind,
        parent: Option<&'static str>,
        range: (u64, u64),
    }

    #[derive(Clone, Copy)]
    struct DefinitionSpec {
        scope: &'static str,
        name: &'static str,
        range: (u64, u64),
        order: DefinitionOrder,
        visibility: VisibilitySpelling,
        declares: Option<&'static str>,
        module: Option<&'static str>,
    }

    #[derive(Clone, Copy)]
    struct ReferenceSpec {
        scope: &'static str,
        start: u64,
        anchor: PathAnchor,
        path: &'static str,
    }

    #[derive(Clone, Copy)]
    struct ImportSpec {
        scope: &'static str,
        alias: Option<&'static str>,
        anchor: PathAnchor,
        path: &'static str,
        rank: u8,
    }

    const UNITS: [&str; 3] = ["src/lib.rs", "src/a.rs", "src/b.rs"];

    const SCOPES: [ScopeSpec; 5] = [
        ScopeSpec {
            label: "L",
            unit: "src/lib.rs",
            kind: ScopeKind::Module,
            parent: None,
            range: (0, 1000),
        },
        ScopeSpec {
            label: "A",
            unit: "src/a.rs",
            kind: ScopeKind::Module,
            parent: None,
            range: (0, 1000),
        },
        ScopeSpec {
            label: "B",
            unit: "src/b.rs",
            kind: ScopeKind::Module,
            parent: None,
            range: (0, 1000),
        },
        ScopeSpec {
            label: "block",
            unit: "src/lib.rs",
            kind: ScopeKind::Block,
            parent: Some("L"),
            range: (30, 60),
        },
        ScopeSpec {
            label: "inline",
            unit: "src/lib.rs",
            kind: ScopeKind::Module,
            parent: Some("L"),
            range: (200, 300),
        },
    ];

    const DEFINITIONS: [DefinitionSpec; 11] = [
        DefinitionSpec {
            scope: "L",
            name: "a",
            range: (1, 7),
            order: DefinitionOrder::Item,
            visibility: PRIVATE,
            declares: None,
            module: Some("src/a.rs"),
        },
        DefinitionSpec {
            scope: "L",
            name: "b",
            range: (8, 14),
            order: DefinitionOrder::Item,
            visibility: PRIVATE,
            declares: None,
            module: Some("src/b.rs"),
        },
        DefinitionSpec {
            scope: "L",
            name: "f",
            range: (15, 25),
            order: DefinitionOrder::Item,
            visibility: PUBLIC,
            declares: None,
            module: None,
        },
        DefinitionSpec {
            scope: "block",
            name: "f",
            range: (39, 40),
            order: DefinitionOrder::Sequential(40),
            visibility: PRIVATE,
            declares: None,
            module: None,
        },
        DefinitionSpec {
            scope: "A",
            name: "g",
            range: (0, 10),
            order: DefinitionOrder::Item,
            visibility: PUBLIC,
            declares: None,
            module: None,
        },
        DefinitionSpec {
            scope: "A",
            name: "h",
            range: (10, 20),
            order: DefinitionOrder::Item,
            visibility: PUBLIC,
            declares: None,
            module: None,
        },
        DefinitionSpec {
            scope: "A",
            name: "dup",
            range: (20, 30),
            order: DefinitionOrder::Item,
            visibility: PUBLIC,
            declares: None,
            module: None,
        },
        DefinitionSpec {
            scope: "A",
            name: "dup",
            range: (30, 40),
            order: DefinitionOrder::Item,
            visibility: PUBLIC,
            declares: None,
            module: None,
        },
        DefinitionSpec {
            scope: "B",
            name: "h",
            range: (0, 10),
            order: DefinitionOrder::Item,
            visibility: PUBLIC,
            declares: None,
            module: None,
        },
        DefinitionSpec {
            scope: "L",
            name: "inl",
            range: (195, 300),
            order: DefinitionOrder::Item,
            visibility: PRIVATE,
            declares: Some("inline"),
            module: None,
        },
        DefinitionSpec {
            scope: "inline",
            name: "k",
            range: (205, 215),
            order: DefinitionOrder::Item,
            visibility: PUBLIC,
            declares: None,
            module: None,
        },
    ];

    const REFERENCES: [ReferenceSpec; 7] = [
        ReferenceSpec {
            scope: "block",
            start: 50,
            anchor: PathAnchor::Lexical,
            path: "f",
        },
        ReferenceSpec {
            scope: "block",
            start: 35,
            anchor: PathAnchor::Lexical,
            path: "f",
        },
        ReferenceSpec {
            scope: "L",
            start: 70,
            anchor: PathAnchor::Lexical,
            path: "a::g",
        },
        ReferenceSpec {
            scope: "L",
            start: 80,
            anchor: PathAnchor::Lexical,
            path: "h",
        },
        ReferenceSpec {
            scope: "A",
            start: 50,
            anchor: PathAnchor::Lexical,
            path: "dup",
        },
        ReferenceSpec {
            scope: "B",
            start: 20,
            anchor: PathAnchor::Lexical,
            path: "g",
        },
        ReferenceSpec {
            scope: "L",
            start: 90,
            anchor: PathAnchor::Lexical,
            path: "inl::k",
        },
    ];

    const IMPORTS: [ImportSpec; 3] = [
        ImportSpec {
            scope: "L",
            alias: Some("h"),
            anchor: PathAnchor::SelfModule,
            path: "a::h",
            rank: 0,
        },
        ImportSpec {
            scope: "L",
            alias: None,
            anchor: PathAnchor::SelfModule,
            path: "b",
            rank: 1,
        },
        ImportSpec {
            scope: "B",
            alias: Some("g"),
            anchor: PathAnchor::Super(1),
            path: "a::g",
            rank: 0,
        },
    ];

    /// Fisher-Yates over an xorshift64 stream; seed zero yields the identity order.
    fn permutation(len: usize, seed: u64) -> Vec<usize> {
        let mut order: Vec<usize> = (0..len).collect();
        let mut state = seed;
        for index in (1..len).rev() {
            if state == 0 {
                break;
            }
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let modulus = u64::try_from(index + 1).unwrap_or(1);
            let swap_with = usize::try_from(state % modulus).unwrap_or(0);
            order.swap(index, swap_with);
        }
        order
    }

    fn build_shuffled(seed: u64) -> BindingGraph {
        let mut fixture = Fixture::new();
        let mut units = BTreeMap::new();
        for index in permutation(UNITS.len(), seed) {
            units.insert(UNITS[index], fixture.unit(UNITS[index]));
        }
        let scope_order = permutation(SCOPES.len(), seed.wrapping_add(1));
        let scope_ids: BTreeMap<&str, ScopeId> = scope_order
            .iter()
            .enumerate()
            .filter_map(|(position, index)| {
                Some((SCOPES[*index].label, ScopeId::from_index(position)?))
            })
            .collect();
        for index in scope_order {
            let spec = SCOPES[index];
            let parent = spec.parent.map(|label| scope_ids[label]);
            let minted = fixture.scope(
                units[spec.unit],
                spec.kind,
                parent,
                spec.range.0,
                spec.range.1,
            );
            assert_eq!(minted, scope_ids[spec.label]);
        }
        let mut definitions = BTreeMap::new();
        for index in permutation(DEFINITIONS.len(), seed.wrapping_add(2)) {
            let spec = DEFINITIONS[index];
            let mut definition = crate::graph::Definition::new(
                scope_ids[spec.scope],
                crate::fixture::name(spec.name),
                crate::fixture::range(spec.range.0, spec.range.1),
                crate::fixture::kind(),
                spec.order,
                spec.visibility,
            );
            if let Some(declares) = spec.declares {
                definition = definition.declaring(scope_ids[declares]);
            }
            let id = fixture
                .builder
                .definition(definition)
                .unwrap_or_else(|error| panic!("{error}"));
            definitions.insert(index, (id, spec));
        }
        for index in permutation(REFERENCES.len(), seed.wrapping_add(3)) {
            let spec = REFERENCES[index];
            fixture.reference(scope_ids[spec.scope], spec.start, spec.anchor, spec.path);
        }
        for index in permutation(IMPORTS.len(), seed.wrapping_add(4)) {
            let spec = IMPORTS[index];
            fixture.import(
                scope_ids[spec.scope],
                spec.alias,
                spec.anchor,
                spec.path,
                spec.rank,
                PUBLIC,
            );
        }
        for (id, spec) in definitions.values() {
            if let Some(module) = spec.module {
                fixture.module_link(scope_ids[spec.scope], *id, module);
            }
        }
        fixture.build()
    }

    fn render(graph: &BindingGraph) -> String {
        let set = resolve(graph, &BindingLimits::default());
        let mut lines: Vec<String> = graph
            .reference_ids()
            .map(|id| render_reference(graph, &set, id))
            .collect();
        lines.sort();
        lines.join("\n")
    }

    fn render_reference(
        graph: &BindingGraph,
        set: &super::ResolutionSet,
        id: ReferenceId,
    ) -> String {
        let reference = graph.reference(id);
        let unit = graph.unit(graph.scope(reference.scope()).unit()).source();
        let mut targets: Vec<String> = set
            .resolution(id)
            .targets()
            .iter()
            .map(|target| {
                let definition = graph.definition(*target);
                let unit = graph.unit(graph.scope(definition.scope()).unit()).source();
                format!("{unit}@{}", definition.range().start())
            })
            .collect();
        targets.sort();
        format!(
            "{unit}@{} -> {}",
            reference.range().start(),
            targets.join(",")
        )
    }

    #[test]
    fn test_resolve_shuffled_input_order_yields_byte_equal_output() {
        let expected = render(&build_shuffled(0));
        assert!(expected.contains("src/lib.rs@50 -> rift://source/project/src/lib.rs@39"));
        assert!(expected.contains("src/lib.rs@35 -> rift://source/project/src/lib.rs@15"));
        assert!(expected.contains("src/lib.rs@70 -> rift://source/project/src/a.rs@0"));
        assert!(expected.contains("src/lib.rs@80 -> rift://source/project/src/a.rs@10"));
        assert!(expected.contains(
            "src/a.rs@50 -> rift://source/project/src/a.rs@20,rift://source/project/src/a.rs@30"
        ));
        assert!(expected.contains("src/b.rs@20 -> rift://source/project/src/a.rs@0"));
        assert!(expected.contains("src/lib.rs@90 -> rift://source/project/src/lib.rs@205"));
        for seed in 1..=8 {
            assert_eq!(render(&build_shuffled(seed)), expected, "seed {seed}");
        }
    }

    #[test]
    fn test_permutation_seed_zero_is_identity_and_seeds_reorder() {
        assert_eq!(permutation(4, 0), vec![0, 1, 2, 3]);
        let shuffled = permutation(8, 7);
        let mut sorted = shuffled.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..8).collect::<Vec<_>>());
        assert_ne!(shuffled, sorted);
    }
}
