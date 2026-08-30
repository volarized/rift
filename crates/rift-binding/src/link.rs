//! Linking: module links across units, the module tree, anchors, visibility, member scopes.
//!
//! [`LinkedGraph::link`] runs one pass per table. Module links join `mod` definitions to
//! the unit scopes they declare; the module tree gives every scope its enclosing module,
//! parent module, and crate root; visibility spellings become the scopes they name; and
//! each member link's owner path is resolved with definitions and imports alone, attaching
//! the member scope to every owner definition it names.

use std::collections::BTreeMap;

use crate::failure::{BindingError, BindingViolation, binding_error};
use crate::graph::{
    BindingGraph, Definition, DefinitionId, LinkId, LinkKind, NamePath, PathAnchor, ScopeId,
    ScopeKind, Visibility, VisibilitySpelling,
};
use crate::limits::{BindingLimits, ExhaustedLimit};
use crate::resolve::{LookupContext, MemberScopes, NeverCancelled, Resolver};

/// Enclosing module, parent module, and crate root of every scope.
#[derive(Debug)]
struct ModuleTree {
    enclosing: Vec<Option<ScopeId>>,
    parent: Vec<Option<ScopeId>>,
    root: Vec<Option<ScopeId>>,
}

impl ModuleTree {
    /// Computes what lexical parents alone establish: every module's parent inside its unit.
    fn lexical(graph: &BindingGraph) -> Self {
        let enclosing = enclosing_modules(graph);
        let parent = graph
            .scopes()
            .iter()
            .map(|scope| match scope.kind() {
                ScopeKind::Module => scope.parent().and_then(|parent| enclosing[parent.index()]),
                ScopeKind::Block | ScopeKind::Member => None,
            })
            .collect();
        Self {
            root: vec![None; graph.scopes().len()],
            enclosing,
            parent,
        }
    }

    /// Makes the module around `declaring` the parent of unit scope `target`, unless the
    /// target already has a parent or sits above that module, which would close a cycle.
    fn declare(
        &mut self,
        declaring: ScopeId,
        target: ScopeId,
        depth_max: usize,
    ) -> Result<(), BindingError> {
        let Some(module) = self.enclosing[declaring.index()] else {
            return Ok(());
        };
        if self.parent[target.index()].is_some() {
            return Ok(());
        }
        match self.reaches(module, target, depth_max) {
            Some(true) => Ok(()),
            Some(false) => {
                self.parent[target.index()] = Some(module);
                Ok(())
            }
            None => Err(path_depth_error(module)),
        }
    }

    /// Whether the parent chain from `from` reaches `target` within `depth_max` steps.
    fn reaches(&self, from: ScopeId, target: ScopeId, depth_max: usize) -> Option<bool> {
        let mut current = from;
        for _ in 0..=depth_max {
            if current == target {
                return Some(true);
            }
            match self.parent[current.index()] {
                Some(parent) => current = parent,
                None => return Some(false),
            }
        }
        None
    }

    /// The top of the parent chain above `module`, within `depth_max` steps.
    fn root_of(&self, module: ScopeId, depth_max: usize) -> Option<ScopeId> {
        let mut current = module;
        for _ in 0..=depth_max {
            match self.parent[current.index()] {
                Some(parent) => current = parent,
                None => return Some(current),
            }
        }
        None
    }

    /// Records every scope's crate root once module links are in place.
    fn finish_roots(&mut self, graph: &BindingGraph, depth_max: usize) -> Result<(), BindingError> {
        for module in graph.scope_ids() {
            if graph.scope(module).kind() != ScopeKind::Module {
                continue;
            }
            let root = self
                .root_of(module, depth_max)
                .ok_or_else(|| path_depth_error(module))?;
            self.root[module.index()] = Some(root);
        }
        let roots = graph
            .scope_ids()
            .map(|scope| self.enclosing[scope.index()].and_then(|module| self.root[module.index()]))
            .collect();
        self.root = roots;
        Ok(())
    }

    /// The module `levels` parents above the module enclosing `scope`.
    fn ancestor(&self, scope: ScopeId, levels: u8) -> Option<ScopeId> {
        let mut current = self.enclosing[scope.index()]?;
        for _ in 0..levels {
            current = self.parent[current.index()]?;
        }
        Some(current)
    }

    /// Whether `container` is `scope` itself or a module above it. Every module chain
    /// reached its root within `path_depth_max` in `finish_roots`, which bounds the walk.
    fn encloses(&self, container: ScopeId, scope: ScopeId) -> bool {
        if container == scope {
            return true;
        }
        let mut current = self.enclosing[scope.index()];
        while let Some(module) = current {
            if module == container {
                return true;
            }
            current = self.parent[module.index()];
        }
        false
    }

    /// The scope a visibility spelling at `scope` names.
    fn visibility(&self, spelling: VisibilitySpelling, scope: ScopeId) -> Visibility {
        let module = self.enclosing[scope.index()].unwrap_or(scope);
        match spelling {
            VisibilitySpelling::Public => Visibility::Public,
            VisibilitySpelling::Private => Visibility::Within(module),
            VisibilitySpelling::Super => {
                Visibility::Within(self.parent[module.index()].unwrap_or(module))
            }
            VisibilitySpelling::Crate => {
                Visibility::Within(self.root[scope.index()].unwrap_or(module))
            }
        }
    }
}

fn path_depth_error(module: ScopeId) -> BindingError {
    binding_error(
        BindingViolation::GraphLimit(ExhaustedLimit::PathDepth),
        format!("the module chain above {module:?} exceeds path_depth_max"),
    )
}

/// The nearest module at or above every scope, computed once per scope.
fn enclosing_modules(graph: &BindingGraph) -> Vec<Option<ScopeId>> {
    let mut modules = vec![None; graph.scopes().len()];
    let mut computed = vec![false; graph.scopes().len()];
    let mut trail = Vec::new();
    for start in graph.scope_ids() {
        if computed[start.index()] {
            continue;
        }
        let module = walk_to_module(graph, start, &computed, &modules, &mut trail);
        for scope in trail.drain(..) {
            computed[scope.index()] = true;
            modules[scope.index()] = module;
        }
    }
    modules
}

/// Walks lexical parents from `start` until a module or a computed scope; the builder
/// refused cyclic parents, so the walk ends within the scope count.
fn walk_to_module(
    graph: &BindingGraph,
    start: ScopeId,
    computed: &[bool],
    modules: &[Option<ScopeId>],
    trail: &mut Vec<ScopeId>,
) -> Option<ScopeId> {
    let mut current = Some(start);
    while let Some(scope_id) = current {
        if computed[scope_id.index()] {
            return modules[scope_id.index()];
        }
        trail.push(scope_id);
        let scope = graph.scope(scope_id);
        if scope.kind() == ScopeKind::Module {
            return Some(scope_id);
        }
        current = scope.parent();
    }
    None
}

/// The first parentless module scope of every unit.
fn unit_scopes(graph: &BindingGraph) -> Vec<Option<ScopeId>> {
    let mut by_unit = vec![None; graph.units().len()];
    for scope_id in graph.scope_ids() {
        let scope = graph.scope(scope_id);
        let is_unit_scope = scope.kind() == ScopeKind::Module && scope.parent().is_none();
        let slot = &mut by_unit[scope.unit().index()];
        if is_unit_scope && slot.is_none() {
            *slot = Some(scope_id);
        }
    }
    by_unit
}

struct ModuleLinks {
    target: Vec<Option<ScopeId>>,
    unlinked: Vec<LinkId>,
}

/// Points every module link at the unit scope it declares and grows the module tree.
fn link_modules(
    graph: &BindingGraph,
    limits: &BindingLimits,
    unit_scopes: &[Option<ScopeId>],
    tree: &mut ModuleTree,
) -> Result<ModuleLinks, BindingError> {
    let mut units = BTreeMap::new();
    for id in graph.unit_ids() {
        units.entry(graph.unit(id).source()).or_insert(id);
    }
    let mut links = ModuleLinks {
        target: vec![None; graph.links().len()],
        unlinked: Vec::new(),
    };
    for link_id in graph.link_ids() {
        let LinkKind::Module { definition, unit } = graph.link(link_id).kind() else {
            continue;
        };
        let target = units.get(unit).and_then(|id| unit_scopes[id.index()]);
        let Some(target) = target else {
            links.unlinked.push(link_id);
            continue;
        };
        links.target[link_id.index()] = Some(target);
        let declaring = graph.definition(*definition).scope();
        tree.declare(declaring, target, limits.path_depth_max())?;
    }
    Ok(links)
}

/// The scope each definition opens: its declared scope, else its module link's target.
fn declared_scopes(
    graph: &BindingGraph,
    module_target: &[Option<ScopeId>],
) -> Vec<Option<ScopeId>> {
    let mut declared: Vec<Option<ScopeId>> = graph
        .definitions()
        .iter()
        .map(Definition::declares)
        .collect();
    for link_id in graph.link_ids() {
        let LinkKind::Module { definition, .. } = graph.link(link_id).kind() else {
            continue;
        };
        let slot = &mut declared[definition.index()];
        if slot.is_none() {
            *slot = module_target[link_id.index()];
        }
    }
    declared
}

struct MemberAttachment {
    link: LinkId,
    scope: ScopeId,
    targets: Vec<DefinitionId>,
}

/// A [`BindingGraph`] with units joined, the module tree computed, and member scopes attached.
#[derive(Debug)]
pub struct LinkedGraph<'graph> {
    graph: &'graph BindingGraph,
    tree: ModuleTree,
    module_target: Vec<Option<ScopeId>>,
    declared_scope: Vec<Option<ScopeId>>,
    member_scopes: Vec<Vec<ScopeId>>,
    visibility: Vec<Visibility>,
    link_visibility: Vec<Option<Visibility>>,
    unlinked_modules: Vec<LinkId>,
    unlinked_members: Vec<LinkId>,
}

impl<'graph> LinkedGraph<'graph> {
    /// Links `graph` under `limits`.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::GraphLimit`] naming `path_depth` when a module chain is
    /// deeper than `path_depth_max`.
    pub fn link(graph: &'graph BindingGraph, limits: &BindingLimits) -> Result<Self, BindingError> {
        let mut tree = ModuleTree::lexical(graph);
        let unit_scopes = unit_scopes(graph);
        let modules = link_modules(graph, limits, &unit_scopes, &mut tree)?;
        tree.finish_roots(graph, limits.path_depth_max())?;
        let declared_scope = declared_scopes(graph, &modules.target);
        let visibility = graph
            .definitions()
            .iter()
            .map(|definition| tree.visibility(definition.visibility(), definition.scope()))
            .collect();
        let link_visibility = graph
            .links()
            .iter()
            .map(|link| match link.kind() {
                LinkKind::Import { visibility, .. } => {
                    Some(tree.visibility(*visibility, link.scope()))
                }
                LinkKind::Member { .. } | LinkKind::Module { .. } => None,
            })
            .collect();
        let mut linked = Self {
            graph,
            tree,
            module_target: modules.target,
            declared_scope,
            member_scopes: vec![Vec::new(); graph.definitions().len()],
            visibility,
            link_visibility,
            unlinked_modules: modules.unlinked,
            unlinked_members: Vec::new(),
        };
        let attachments = linked.member_attachments(limits)?;
        linked.attach(attachments);
        Ok(linked)
    }

    /// Returns the linked graph.
    #[must_use]
    pub const fn graph(&self) -> &'graph BindingGraph {
        self.graph
    }

    /// The unit scope a module link declares, once linked.
    #[must_use]
    pub fn module_target(&self, link: LinkId) -> Option<ScopeId> {
        self.module_target[link.index()]
    }

    /// The nearest module scope at or above `scope`.
    #[must_use]
    pub fn enclosing_module(&self, scope: ScopeId) -> Option<ScopeId> {
        self.tree.enclosing[scope.index()]
    }

    /// The module above a module scope, inside its unit or through a module link.
    #[must_use]
    pub fn parent_module(&self, scope: ScopeId) -> Option<ScopeId> {
        self.tree.parent[scope.index()]
    }

    /// The top of the module chain above `scope`.
    #[must_use]
    pub fn crate_root(&self, scope: ScopeId) -> Option<ScopeId> {
        self.tree.root[scope.index()]
    }

    /// The scope where a path anchored at `anchor` starts when it occurs in `scope`.
    #[must_use]
    pub fn anchor_scope(&self, scope: ScopeId, anchor: PathAnchor) -> Option<ScopeId> {
        match anchor {
            PathAnchor::Lexical => Some(scope),
            PathAnchor::SelfModule => self.enclosing_module(scope),
            PathAnchor::Crate => self.crate_root(scope),
            PathAnchor::Super(levels) => self.tree.ancestor(scope, levels),
        }
    }

    /// The scopes a definition can be named from.
    #[must_use]
    pub fn visibility(&self, definition: DefinitionId) -> Visibility {
        self.visibility[definition.index()]
    }

    /// Whether `definition` can be named from `from`.
    #[must_use]
    pub fn visible(&self, definition: DefinitionId, from: ScopeId) -> bool {
        self.within(self.visibility(definition), from)
    }

    /// Whether an import link can be followed from `from`; non-import links always can.
    pub(crate) fn link_visible(&self, link: LinkId, from: ScopeId) -> bool {
        self.link_visibility[link.index()].is_none_or(|visibility| self.within(visibility, from))
    }

    fn within(&self, visibility: Visibility, from: ScopeId) -> bool {
        match visibility {
            Visibility::Public => true,
            Visibility::Within(container) => self.tree.encloses(container, from),
        }
    }

    /// The scope a definition opens, where it opens one.
    #[must_use]
    pub fn declared_scope(&self, definition: DefinitionId) -> Option<ScopeId> {
        self.declared_scope[definition.index()]
    }

    /// The member scopes attached to a definition, in member link order.
    #[must_use]
    pub fn member_scopes(&self, definition: DefinitionId) -> &[ScopeId] {
        &self.member_scopes[definition.index()]
    }

    /// The declared scope, then the member scopes when `members` follows them.
    pub(crate) fn opened_scopes(
        &self,
        definition: DefinitionId,
        members: MemberScopes,
    ) -> impl Iterator<Item = ScopeId> + '_ {
        let attached = match members {
            MemberScopes::Follow => self.member_scopes(definition),
            MemberScopes::Ignore => &[],
        };
        self.declared_scope(definition)
            .into_iter()
            .chain(attached.iter().copied())
    }

    /// Module links whose unit is not in the graph, in link order.
    #[must_use]
    pub fn unlinked_modules(&self) -> &[LinkId] {
        &self.unlinked_modules
    }

    /// Member links whose owner resolved to nothing, in link order.
    #[must_use]
    pub fn unlinked_members(&self) -> &[LinkId] {
        &self.unlinked_members
    }

    fn member_attachments(
        &self,
        limits: &BindingLimits,
    ) -> Result<Vec<MemberAttachment>, BindingError> {
        let mut resolver = Resolver::new(self, limits, &NeverCancelled);
        let mut attachments = Vec::new();
        for link_id in self.graph.link_ids() {
            let link = self.graph.link(link_id);
            let LinkKind::Member {
                owner_anchor,
                owner,
                scope,
            } = link.kind()
            else {
                continue;
            };
            let targets =
                self.owner_targets(&mut resolver, link.scope(), *owner_anchor, owner, *scope)?;
            attachments.push(MemberAttachment {
                link: link_id,
                scope: *scope,
                targets,
            });
        }
        Ok(attachments)
    }

    /// Resolves one member link's owner with definitions and imports alone, judging
    /// sequential order from the member scope's start.
    fn owner_targets(
        &self,
        resolver: &mut Resolver<'_>,
        at: ScopeId,
        anchor: PathAnchor,
        owner: &NamePath,
        member: ScopeId,
    ) -> Result<Vec<DefinitionId>, BindingError> {
        let Some(start) = self.anchor_scope(at, anchor) else {
            return Ok(Vec::new());
        };
        let context = LookupContext {
            position: self.graph.scope(member).range().start(),
            members: MemberScopes::Ignore,
        };
        let resolution = resolver.resolve_path(start, anchor, at, owner, context)?;
        Ok(resolution.targets().to_vec())
    }

    fn attach(&mut self, attachments: Vec<MemberAttachment>) {
        for attachment in attachments {
            if attachment.targets.is_empty() {
                self.unlinked_members.push(attachment.link);
            }
            for target in attachment.targets {
                self.member_scopes[target.index()].push(attachment.scope);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LinkedGraph;
    use crate::failure::{BindingError, BindingViolation};
    use crate::fixture::Fixture;
    use crate::graph::{PathAnchor, Visibility, VisibilitySpelling};
    use crate::limits::{BindingLimits, ExhaustedLimit};

    fn violation<T>(result: Result<T, BindingError>) -> Option<BindingViolation> {
        result.err().map(|error| error.fault().violation())
    }

    #[test]
    fn test_link_module_link_targets_unit_scope() {
        let mut fixture = Fixture::new();
        let main = fixture.unit("src/main.rs");
        let other = fixture.unit("src/a.rs");
        let root = fixture.module(main, None, 0, 100);
        let other_root = fixture.module(other, None, 0, 100);
        let declaration = fixture.item(root, "a", 0, 6, VisibilitySpelling::Private);
        let link = fixture.module_link(root, declaration, "src/a.rs");
        let graph = fixture.build();
        let linked = LinkedGraph::link(&graph, &BindingLimits::default()).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert_eq!(linked.module_target(link), Some(other_root));
        assert_eq!(linked.parent_module(other_root), Some(root));
        assert_eq!(linked.parent_module(root), None);
        assert_eq!(linked.crate_root(other_root), Some(root));
        assert_eq!(linked.declared_scope(declaration), Some(other_root));
        assert_eq!(linked.unlinked_modules(), &[]);
        assert_eq!(linked.unlinked_members(), &[]);
        assert!(std::ptr::eq(linked.graph(), &raw const graph));
    }

    #[test]
    fn test_link_unknown_unit_recorded_unlinked() {
        let mut fixture = Fixture::new();
        let main = fixture.unit("src/main.rs");
        let root = fixture.module(main, None, 0, 100);
        let declaration = fixture.item(root, "a", 0, 6, VisibilitySpelling::Private);
        let link = fixture.module_link(root, declaration, "src/missing.rs");
        let graph = fixture.build();
        let linked = LinkedGraph::link(&graph, &BindingLimits::default()).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert_eq!(linked.unlinked_modules(), &[link]);
        assert_eq!(linked.module_target(link), None);
        assert_eq!(linked.declared_scope(declaration), None);
    }

    #[test]
    fn test_link_member_link_attaches_scope_to_owner() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let owner = fixture.item(root, "T", 0, 10, VisibilitySpelling::Public);
        let body = fixture.member(unit, Some(root), 20, 60);
        let first = fixture.member_link(root, PathAnchor::Lexical, "T", body);
        let second_body = fixture.member(unit, Some(root), 70, 90);
        fixture.member_link(root, PathAnchor::SelfModule, "T", second_body);
        let graph = fixture.build();
        let linked = LinkedGraph::link(&graph, &BindingLimits::default()).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert_eq!(linked.member_scopes(owner), &[body, second_body]);
        assert_eq!(linked.unlinked_members(), &[]);
        assert_eq!(linked.module_target(first), None);
    }

    #[test]
    fn test_link_member_link_unresolved_owner_recorded_unlinked() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let owner = fixture.item(root, "T", 0, 10, VisibilitySpelling::Public);
        let body = fixture.member(unit, Some(root), 20, 60);
        let missing = fixture.member_link(root, PathAnchor::Lexical, "Missing", body);
        let past_chain = fixture.member_link(root, PathAnchor::Super(3), "T", body);
        let graph = fixture.build();
        let linked = LinkedGraph::link(&graph, &BindingLimits::default()).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert_eq!(linked.unlinked_members(), &[missing, past_chain]);
        assert_eq!(linked.member_scopes(owner), &[]);
    }

    #[test]
    fn test_link_anchors_resolve_from_nested_block() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let inner = fixture.module(unit, Some(root), 10, 80);
        let block = fixture.block(unit, Some(inner), 20, 70);
        let graph = fixture.build();
        let linked = LinkedGraph::link(&graph, &BindingLimits::default()).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert_eq!(linked.anchor_scope(block, PathAnchor::Lexical), Some(block));
        assert_eq!(
            linked.anchor_scope(block, PathAnchor::SelfModule),
            Some(inner)
        );
        assert_eq!(linked.anchor_scope(block, PathAnchor::Crate), Some(root));
        assert_eq!(
            linked.anchor_scope(block, PathAnchor::Super(0)),
            Some(inner)
        );
        assert_eq!(linked.anchor_scope(block, PathAnchor::Super(1)), Some(root));
        assert_eq!(linked.anchor_scope(block, PathAnchor::Super(2)), None);
        assert_eq!(linked.enclosing_module(block), Some(inner));
        assert_eq!(linked.enclosing_module(inner), Some(inner));
        assert_eq!(linked.parent_module(inner), Some(root));
        assert_eq!(linked.parent_module(block), None);
        assert_eq!(linked.crate_root(block), Some(root));
    }

    #[test]
    fn test_link_visibility_spellings_name_containers() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let inner = fixture.module(unit, Some(root), 10, 80);
        let block = fixture.block(unit, Some(inner), 20, 70);
        let public = fixture.item(inner, "a", 11, 12, VisibilitySpelling::Public);
        let crate_wide = fixture.item(inner, "b", 13, 14, VisibilitySpelling::Crate);
        let parent_wide = fixture.item(inner, "c", 15, 16, VisibilitySpelling::Super);
        let private = fixture.item(inner, "d", 17, 18, VisibilitySpelling::Private);
        let graph = fixture.build();
        let linked = LinkedGraph::link(&graph, &BindingLimits::default()).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert_eq!(linked.visibility(public), Visibility::Public);
        assert_eq!(linked.visibility(crate_wide), Visibility::Within(root));
        assert_eq!(linked.visibility(parent_wide), Visibility::Within(root));
        assert_eq!(linked.visibility(private), Visibility::Within(inner));
        for definition in [public, crate_wide, parent_wide, private] {
            assert!(
                linked.visible(definition, block),
                "own block sees {definition:?}"
            );
            assert!(
                linked.visible(definition, inner),
                "own module sees {definition:?}"
            );
        }
        assert!(linked.visible(public, root));
        assert!(linked.visible(crate_wide, root));
        assert!(linked.visible(parent_wide, root));
        assert!(!linked.visible(private, root));
    }

    #[test]
    fn test_link_module_declaration_cycle_keeps_first_parent() {
        let mut fixture = Fixture::new();
        let first = fixture.unit("src/a.rs");
        let second = fixture.unit("src/b.rs");
        let first_root = fixture.module(first, None, 0, 100);
        let second_root = fixture.module(second, None, 0, 100);
        let declares_second = fixture.item(first_root, "b", 0, 6, VisibilitySpelling::Private);
        let declares_first = fixture.item(second_root, "a", 0, 6, VisibilitySpelling::Private);
        let to_second = fixture.module_link(first_root, declares_second, "src/b.rs");
        let to_first = fixture.module_link(second_root, declares_first, "src/a.rs");
        let graph = fixture.build();
        let linked = LinkedGraph::link(&graph, &BindingLimits::default()).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert_eq!(linked.module_target(to_second), Some(second_root));
        assert_eq!(linked.module_target(to_first), Some(first_root));
        assert_eq!(linked.parent_module(second_root), Some(first_root));
        assert_eq!(linked.parent_module(first_root), None);
        assert_eq!(linked.crate_root(second_root), Some(first_root));
        assert_eq!(linked.crate_root(first_root), Some(first_root));
    }

    #[test]
    fn test_link_unit_declared_twice_keeps_first_parent() {
        let mut fixture = Fixture::new();
        let first = fixture.unit("src/a.rs");
        let second = fixture.unit("src/c.rs");
        let shared = fixture.unit("src/b.rs");
        let first_root = fixture.module(first, None, 0, 100);
        let second_root = fixture.module(second, None, 0, 100);
        let shared_root = fixture.module(shared, None, 0, 100);
        let from_first = fixture.item(first_root, "b", 0, 6, VisibilitySpelling::Private);
        let from_second = fixture.item(second_root, "b", 0, 6, VisibilitySpelling::Private);
        fixture.module_link(first_root, from_first, "src/b.rs");
        fixture.module_link(second_root, from_second, "src/b.rs");
        let graph = fixture.build();
        let linked = LinkedGraph::link(&graph, &BindingLimits::default()).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert_eq!(linked.parent_module(shared_root), Some(first_root));
        assert_eq!(linked.declared_scope(from_second), Some(shared_root));
    }

    #[test]
    fn test_link_deep_inline_module_chain_refused_path_depth() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let mut parent = fixture.module(unit, None, 0, 100);
        for depth in 1..=4 {
            parent = fixture.module(unit, Some(parent), depth, depth + 50);
        }
        let graph = fixture.build();
        let limits = BindingLimits::builder().path_depth_max(2).build();
        let refused = LinkedGraph::link(&graph, &limits.unwrap_or_default());
        let expected = BindingViolation::GraphLimit(ExhaustedLimit::PathDepth);
        assert_eq!(violation(refused), Some(expected));
        let accepted = LinkedGraph::link(&graph, &BindingLimits::default());
        assert!(
            accepted.is_ok(),
            "the default depth accepts four nested modules"
        );
    }

    #[test]
    fn test_link_deep_declaration_chain_refused_path_depth() {
        let mut fixture = Fixture::new();
        let names = ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"];
        let mut roots = Vec::new();
        for path in names {
            let unit = fixture.unit(path);
            roots.push(fixture.module(unit, None, 0, 100));
        }
        for (index, pair) in roots.windows(2).enumerate() {
            let declaration = fixture.item(pair[0], "child", 0, 6, VisibilitySpelling::Private);
            fixture.module_link(pair[0], declaration, names[index + 1]);
        }
        let graph = fixture.build();
        let limits = BindingLimits::builder().path_depth_max(1).build();
        let refused = LinkedGraph::link(&graph, &limits.unwrap_or_default());
        let expected = BindingViolation::GraphLimit(ExhaustedLimit::PathDepth);
        assert_eq!(violation(refused), Some(expected));
    }

    #[test]
    fn test_link_block_only_unit_has_no_module_anchors() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let block = fixture.block(unit, None, 0, 100);
        let nested = fixture.block(unit, Some(block), 10, 20);
        let private = fixture.item(block, "f", 0, 6, VisibilitySpelling::Private);
        let crate_wide = fixture.item(block, "g", 7, 8, VisibilitySpelling::Crate);
        let graph = fixture.build();
        let linked = LinkedGraph::link(&graph, &BindingLimits::default()).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert_eq!(linked.enclosing_module(nested), None);
        assert_eq!(linked.crate_root(nested), None);
        assert_eq!(linked.anchor_scope(nested, PathAnchor::SelfModule), None);
        assert_eq!(linked.anchor_scope(nested, PathAnchor::Crate), None);
        assert_eq!(linked.visibility(private), Visibility::Within(block));
        assert_eq!(linked.visibility(crate_wide), Visibility::Within(block));
        assert!(linked.visible(private, block));
        assert!(!linked.visible(private, nested));
    }

    #[test]
    fn test_link_import_visibility_follows_spelling() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let inner = fixture.module(unit, Some(root), 10, 80);
        let block = fixture.block(unit, Some(inner), 20, 70);
        let private = fixture.import(
            inner,
            Some("f"),
            PathAnchor::Crate,
            "f",
            0,
            VisibilitySpelling::Private,
        );
        let public = fixture.import(
            inner,
            Some("g"),
            PathAnchor::Crate,
            "g",
            0,
            VisibilitySpelling::Public,
        );
        let member = fixture.member(unit, Some(root), 90, 95);
        let attached = fixture.member_link(root, PathAnchor::Lexical, "T", member);
        let graph = fixture.build();
        let linked = LinkedGraph::link(&graph, &BindingLimits::default()).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert!(linked.link_visible(private, block));
        assert!(!linked.link_visible(private, root));
        assert!(linked.link_visible(public, root));
        assert!(linked.link_visible(attached, root));
    }

    #[test]
    fn test_link_module_declared_from_block_only_unit_gets_no_parent() {
        let mut fixture = Fixture::new();
        let first = fixture.unit("src/a.rs");
        let second = fixture.unit("src/b.rs");
        let block = fixture.block(first, None, 0, 100);
        let second_root = fixture.module(second, None, 0, 100);
        let declaration = fixture.item(block, "b", 0, 6, VisibilitySpelling::Private);
        let link = fixture.module_link(block, declaration, "src/b.rs");
        let graph = fixture.build();
        let linked = LinkedGraph::link(&graph, &BindingLimits::default()).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert_eq!(linked.module_target(link), Some(second_root));
        assert_eq!(linked.parent_module(second_root), None);
        assert_eq!(linked.crate_root(second_root), Some(second_root));
    }

    #[test]
    fn test_link_member_link_owner_path_through_declared_scope() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 200);
        let m_body = fixture.module(unit, Some(root), 10, 80);
        fixture.declaring_item(root, "m", 5, 80, VisibilitySpelling::Public, m_body);
        let owner = fixture.item(m_body, "T", 11, 20, VisibilitySpelling::Public);
        let body = fixture.member(unit, Some(root), 100, 150);
        fixture.member_link(root, PathAnchor::Lexical, "m::T", body);
        let graph = fixture.build();
        let linked = LinkedGraph::link(&graph, &BindingLimits::default()).ok();
        let Some(linked) = linked else {
            panic!("linking succeeds");
        };
        assert_eq!(linked.member_scopes(owner), &[body]);
        assert_eq!(linked.unlinked_members(), &[]);
    }
}
