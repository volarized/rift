//! Test fixtures: a small builder DSL over the public graph types.

use rift_core::{
    ContributionOrigin, ExactKind, ReferenceRole, SourceKind, SourceLocation, SourceRange,
    SourceUnitId,
};

use crate::graph::{
    BindingGraph, Definition, DefinitionId, DefinitionOrder, GraphBuilder, Link, LinkId, LinkKind,
    Name, NamePath, PathAnchor, Rank, Reference, ReferenceId, Scope, ScopeId, ScopeKind, UnitId,
    VisibilitySpelling,
};
use crate::limits::BindingLimits;
use crate::link::LinkedGraph;
use crate::resolve::{NeverCancelled, ResolutionSet, resolve_all};

pub(crate) fn source(path: &str) -> SourceUnitId {
    SourceUnitId::parse(&format!("rift://source/project/{path}")).expect("fixture unit id parses")
}

pub(crate) fn origin() -> ContributionOrigin {
    let location = SourceLocation::Project { package: None };
    ContributionOrigin::new(Some(location), SourceKind::Authored).expect("authored origin")
}

pub(crate) fn name(text: &str) -> Name {
    Name::new(text).expect("fixture name is valid")
}

pub(crate) fn path(text: &str) -> NamePath {
    NamePath::new(text.split("::").map(name).collect()).expect("fixture path is valid")
}

pub(crate) fn range(start: u64, end: u64) -> SourceRange {
    SourceRange::new(start, end).expect("fixture range is non-empty")
}

pub(crate) fn kind() -> ExactKind {
    ExactKind("rust.item".to_owned())
}

/// Builds graphs for tests; every method panics on a refusal so tests read straight down.
pub(crate) struct Fixture {
    pub(crate) builder: GraphBuilder,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        Self::with_limits(BindingLimits::default())
    }

    pub(crate) fn with_limits(limits: BindingLimits) -> Self {
        Self {
            builder: GraphBuilder::new(limits),
        }
    }

    pub(crate) fn unit(&mut self, path: &str) -> UnitId {
        self.builder
            .unit(source(path), origin())
            .expect("unit accepted")
    }

    pub(crate) fn scope(
        &mut self,
        unit: UnitId,
        kind: ScopeKind,
        parent: Option<ScopeId>,
        start: u64,
        end: u64,
    ) -> ScopeId {
        let scope = Scope::new(unit, range(start, end), kind, parent);
        self.builder.scope(scope).expect("scope accepted")
    }

    pub(crate) fn module(
        &mut self,
        unit: UnitId,
        parent: Option<ScopeId>,
        start: u64,
        end: u64,
    ) -> ScopeId {
        self.scope(unit, ScopeKind::Module, parent, start, end)
    }

    pub(crate) fn block(
        &mut self,
        unit: UnitId,
        parent: Option<ScopeId>,
        start: u64,
        end: u64,
    ) -> ScopeId {
        self.scope(unit, ScopeKind::Block, parent, start, end)
    }

    pub(crate) fn member(
        &mut self,
        unit: UnitId,
        parent: Option<ScopeId>,
        start: u64,
        end: u64,
    ) -> ScopeId {
        self.scope(unit, ScopeKind::Member, parent, start, end)
    }

    pub(crate) fn item(
        &mut self,
        scope: ScopeId,
        text: &str,
        start: u64,
        end: u64,
        visibility: VisibilitySpelling,
    ) -> DefinitionId {
        let definition = Definition::new(
            scope,
            name(text),
            range(start, end),
            kind(),
            DefinitionOrder::Item,
            visibility,
        );
        self.builder
            .definition(definition)
            .expect("definition accepted")
    }

    pub(crate) fn declaring_item(
        &mut self,
        scope: ScopeId,
        text: &str,
        start: u64,
        end: u64,
        visibility: VisibilitySpelling,
        declares: ScopeId,
    ) -> DefinitionId {
        let definition = Definition::new(
            scope,
            name(text),
            range(start, end),
            kind(),
            DefinitionOrder::Item,
            visibility,
        )
        .declaring(declares);
        self.builder
            .definition(definition)
            .expect("definition accepted")
    }

    pub(crate) fn sequential(&mut self, scope: ScopeId, text: &str, position: u64) -> DefinitionId {
        let definition = Definition::new(
            scope,
            name(text),
            range(position.saturating_sub(1), position),
            kind(),
            DefinitionOrder::Sequential(position),
            VisibilitySpelling::Private,
        );
        self.builder
            .definition(definition)
            .expect("definition accepted")
    }

    pub(crate) fn reference(
        &mut self,
        scope: ScopeId,
        start: u64,
        anchor: PathAnchor,
        text: &str,
    ) -> ReferenceId {
        let reference = Reference::new(
            scope,
            range(start, start + 1),
            anchor,
            path(text),
            ReferenceRole::Read,
        );
        self.builder
            .reference(reference)
            .expect("reference accepted")
    }

    pub(crate) fn import(
        &mut self,
        scope: ScopeId,
        alias: Option<&str>,
        anchor: PathAnchor,
        text: &str,
        rank: u8,
        visibility: VisibilitySpelling,
    ) -> LinkId {
        let kind = LinkKind::Import {
            name: alias.map(name),
            anchor,
            path: path(text),
            visibility,
        };
        self.builder
            .link(Link::new(scope, kind, Rank::new(rank)))
            .expect("import accepted")
    }

    pub(crate) fn member_link(
        &mut self,
        scope: ScopeId,
        owner_anchor: PathAnchor,
        owner: &str,
        member: ScopeId,
    ) -> LinkId {
        let kind = LinkKind::Member {
            owner_anchor,
            owner: path(owner),
            scope: member,
        };
        self.builder
            .link(Link::new(scope, kind, Rank::new(0)))
            .expect("member link accepted")
    }

    pub(crate) fn module_link(
        &mut self,
        scope: ScopeId,
        definition: DefinitionId,
        unit: &str,
    ) -> LinkId {
        let kind = LinkKind::Module {
            definition,
            unit: source(unit),
        };
        self.builder
            .link(Link::new(scope, kind, Rank::new(0)))
            .expect("module link accepted")
    }

    pub(crate) fn build(self) -> BindingGraph {
        self.builder.build().expect("graph builds")
    }
}

pub(crate) fn resolve(graph: &BindingGraph, limits: &BindingLimits) -> ResolutionSet {
    let linked = LinkedGraph::link(graph, limits).expect("graph links");
    resolve_all(&linked, limits, &NeverCancelled).expect("resolution completes")
}

pub(crate) fn targets(set: &ResolutionSet, reference: ReferenceId) -> Vec<DefinitionId> {
    set.resolution(reference).targets().to_vec()
}
