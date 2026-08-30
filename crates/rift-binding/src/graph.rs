//! The owned binding graph: units, scopes, definitions, references, links, and its builder.
//!
//! [`GraphBuilder`] mints every id in insertion order, and each id indexes the graph that
//! minted it. Accessors index directly, so an id from another graph is a programmer error.

use std::collections::BTreeMap;
use std::fmt;

use rift_core::{
    ContributionOrigin, ExactKind, LoopBudget, ReferenceRole, SourceRange, SourceUnitId,
    SymbolFacet,
};

use crate::failure::{BindingError, BindingViolation, binding_error};
use crate::limits::{BindingLimits, ExhaustedLimit};

/// Most UTF-8 bytes one [`Name`] may hold.
pub const NAME_BYTES_MAX: usize = 256;
/// Most segments one [`NamePath`] may hold.
pub const NAME_PATH_SEGMENTS_MAX: usize = 32;

macro_rules! define_graph_id {
    ($name:ident, $docs:literal) => {
        #[doc = $docs]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u32);

        impl $name {
            pub(crate) fn from_index(index: usize) -> Option<Self> {
                u32::try_from(index).ok().map(Self)
            }

            pub(crate) const fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

define_graph_id!(UnitId, "Position of one unit in the graph's unit table.");
define_graph_id!(ScopeId, "Identity of one scope.");
define_graph_id!(DefinitionId, "Identity of one definition.");
define_graph_id!(ReferenceId, "Identity of one reference.");
define_graph_id!(LinkId, "Identity of one link.");

/// A non-empty identifier of at most `NAME_BYTES_MAX` bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Name(String);

impl Name {
    /// Validates one identifier.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::InvalidName`] when the text is empty or longer than
    /// `NAME_BYTES_MAX` bytes.
    pub fn new(text: impl Into<String>) -> Result<Self, BindingError> {
        let text = text.into();
        if text.is_empty() || text.len() > NAME_BYTES_MAX {
            let detail = format!("{} bytes, expected 1 to {NAME_BYTES_MAX}", text.len());
            return Err(binding_error(BindingViolation::InvalidName, detail));
        }
        Ok(Self(text))
    }

    /// Returns the identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One to `NAME_PATH_SEGMENTS_MAX` names, outermost first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NamePath(Vec<Name>);

impl NamePath {
    /// Validates one path.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::InvalidPath`] when the path is empty or longer than
    /// `NAME_PATH_SEGMENTS_MAX` segments.
    pub fn new(segments: Vec<Name>) -> Result<Self, BindingError> {
        if segments.is_empty() || segments.len() > NAME_PATH_SEGMENTS_MAX {
            let detail = format!(
                "{} segments, expected 1 to {NAME_PATH_SEGMENTS_MAX}",
                segments.len()
            );
            return Err(binding_error(BindingViolation::InvalidPath, detail));
        }
        Ok(Self(segments))
    }

    /// A path of one name.
    #[must_use]
    pub fn single(name: Name) -> Self {
        Self(vec![name])
    }

    /// Returns the segments, outermost first.
    #[must_use]
    pub fn segments(&self) -> &[Name] {
        &self.0
    }

    /// Returns the first segment; a path always holds one.
    #[must_use]
    pub fn head(&self) -> &Name {
        &self.0[0]
    }

    /// Returns every segment after the first.
    #[must_use]
    pub fn tail(&self) -> &[Name] {
        &self.0[1..]
    }
}

/// What kind of names one scope holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    /// A module: names inside it do not fall through to the enclosing module.
    Module,
    /// A block: names fall through to the lexical parent.
    Block,
    /// A member scope: the associated items of one definition, reached through a path.
    Member,
}

/// When a definition becomes visible to references in its scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefinitionOrder {
    /// Visible anywhere in its scope.
    Item,
    /// Visible to references starting at or after this byte; the latest such definition
    /// shadows earlier ones.
    Sequential(u64),
}

/// Where a definition or import can be named from, after linking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Visibility {
    /// From anywhere.
    Public,
    /// From this scope and the scopes nested in it.
    Within(ScopeId),
}

/// How the language spelled a definition's or import's visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VisibilitySpelling {
    /// `pub`: visible from anywhere.
    Public,
    /// `pub(crate)`: visible within the crate root.
    Crate,
    /// `pub(super)`: visible within the parent module.
    Super,
    /// No modifier: visible within the defining module.
    Private,
}

/// Where a name path's first segment is looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathAnchor {
    /// The crate root of the reference's module.
    Crate,
    /// The nearest enclosing module.
    SelfModule,
    /// This many parent modules above the nearest enclosing module.
    Super(u8),
    /// The reference's own scope, then its lexical parents.
    Lexical,
}

/// One region of a unit that holds names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    unit: UnitId,
    range: SourceRange,
    kind: ScopeKind,
    parent: Option<ScopeId>,
}

impl Scope {
    /// Describes one scope; `parent` is the lexical parent in the same unit.
    #[must_use]
    pub const fn new(
        unit: UnitId,
        range: SourceRange,
        kind: ScopeKind,
        parent: Option<ScopeId>,
    ) -> Self {
        Self {
            unit,
            range,
            kind,
            parent,
        }
    }

    /// Returns the unit holding the scope.
    #[must_use]
    pub const fn unit(&self) -> UnitId {
        self.unit
    }

    /// Returns the scope's byte range.
    #[must_use]
    pub const fn range(&self) -> SourceRange {
        self.range
    }

    /// Returns the scope kind.
    #[must_use]
    pub const fn kind(&self) -> ScopeKind {
        self.kind
    }

    /// Returns the lexical parent, absent for a unit scope.
    #[must_use]
    pub const fn parent(&self) -> Option<ScopeId> {
        self.parent
    }
}

/// One named declaration inside a scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    scope: ScopeId,
    name: Name,
    range: SourceRange,
    kind: ExactKind,
    facets: Vec<SymbolFacet>,
    order: DefinitionOrder,
    visibility: VisibilitySpelling,
    declares: Option<ScopeId>,
    is_item: bool,
}

impl Definition {
    /// Describes one definition; `range` is the whole declaration item.
    #[must_use]
    pub const fn new(
        scope: ScopeId,
        name: Name,
        range: SourceRange,
        kind: ExactKind,
        order: DefinitionOrder,
        visibility: VisibilitySpelling,
    ) -> Self {
        Self {
            scope,
            name,
            range,
            kind,
            facets: Vec::new(),
            order,
            visibility,
            declares: None,
            is_item: false,
        }
    }

    /// Attaches portable facets.
    #[must_use]
    pub fn with_facets(mut self, facets: Vec<SymbolFacet>) -> Self {
        self.facets = facets;
        self
    }

    /// Names the scope this definition opens, such as a module body.
    #[must_use]
    pub const fn declaring(mut self, scope: ScopeId) -> Self {
        self.declares = Some(scope);
        self
    }

    /// Marks the definition as one a syntax declaration shares.
    #[must_use]
    pub const fn item(mut self) -> Self {
        self.is_item = true;
        self
    }

    /// Returns the scope holding the definition.
    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    /// Returns the declared name.
    #[must_use]
    pub const fn name(&self) -> &Name {
        &self.name
    }

    /// Returns the declaration's byte range.
    #[must_use]
    pub const fn range(&self) -> SourceRange {
        self.range
    }

    /// Returns the provider-local kind.
    #[must_use]
    pub const fn kind(&self) -> &ExactKind {
        &self.kind
    }

    /// Returns the portable facets.
    #[must_use]
    pub fn facets(&self) -> &[SymbolFacet] {
        &self.facets
    }

    /// Returns when the definition becomes visible in its scope.
    #[must_use]
    pub const fn order(&self) -> DefinitionOrder {
        self.order
    }

    /// Returns the spelled visibility.
    #[must_use]
    pub const fn visibility(&self) -> VisibilitySpelling {
        self.visibility
    }

    /// Returns the scope the definition opens, where it opens one.
    #[must_use]
    pub const fn declares(&self) -> Option<ScopeId> {
        self.declares
    }

    /// Whether a syntax declaration shares this definition.
    #[must_use]
    pub const fn is_item(&self) -> bool {
        self.is_item
    }
}

/// One occurrence of a name path that names definitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    scope: ScopeId,
    range: SourceRange,
    anchor: PathAnchor,
    path: NamePath,
    role: ReferenceRole,
}

impl Reference {
    /// Describes one reference occurring at `range` inside `scope`.
    #[must_use]
    pub const fn new(
        scope: ScopeId,
        range: SourceRange,
        anchor: PathAnchor,
        path: NamePath,
        role: ReferenceRole,
    ) -> Self {
        Self {
            scope,
            range,
            anchor,
            path,
            role,
        }
    }

    /// Returns the scope holding the reference.
    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    /// Returns the occurrence's byte range.
    #[must_use]
    pub const fn range(&self) -> SourceRange {
        self.range
    }

    /// Returns where the first segment is looked up.
    #[must_use]
    pub const fn anchor(&self) -> PathAnchor {
        self.anchor
    }

    /// Returns the name path.
    #[must_use]
    pub const fn path(&self) -> &NamePath {
        &self.path
    }

    /// Returns the portable role.
    #[must_use]
    pub const fn role(&self) -> ReferenceRole {
        self.role
    }
}

/// Precedence of one link or definition step; `0` is strongest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rank(u8);

impl Rank {
    /// Wraps one precedence value.
    #[must_use]
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Returns the precedence value.
    #[must_use]
    pub const fn value(self) -> u8 {
        self.0
    }
}

/// What one link contributes to resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkKind {
    /// `name` at the link's scope continues with `path` from `anchor`; a `name` of `None`
    /// is a wildcard, and any `x` continues with `path` followed by `x`.
    Import {
        /// The alias or last segment the import binds, or `None` for a wildcard.
        name: Option<Name>,
        /// Where the import path starts.
        anchor: PathAnchor,
        /// The imported path.
        path: NamePath,
        /// How the import's visibility was spelled.
        visibility: VisibilitySpelling,
    },
    /// `scope` holds members of the definition `owner` resolves to from `owner_anchor`.
    Member {
        /// Where the owner path starts.
        owner_anchor: PathAnchor,
        /// The path naming the owner definition.
        owner: NamePath,
        /// The member scope to attach.
        scope: ScopeId,
    },
    /// `definition` declares the unit scope of `unit`.
    Module {
        /// The `mod` definition.
        definition: DefinitionId,
        /// The unit whose unit scope the definition opens.
        unit: SourceUnitId,
    },
}

/// One ranked edge attached to a scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    scope: ScopeId,
    kind: LinkKind,
    rank: Rank,
}

impl Link {
    /// Attaches `kind` to `scope` at `rank`.
    #[must_use]
    pub const fn new(scope: ScopeId, kind: LinkKind, rank: Rank) -> Self {
        Self { scope, kind, rank }
    }

    /// Returns the scope the link hangs from.
    #[must_use]
    pub const fn scope(&self) -> ScopeId {
        self.scope
    }

    /// Returns what the link contributes.
    #[must_use]
    pub const fn kind(&self) -> &LinkKind {
        &self.kind
    }

    /// Returns the link's precedence.
    #[must_use]
    pub const fn rank(&self) -> Rank {
        self.rank
    }
}

/// One unit in the graph's unit table.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    source: SourceUnitId,
    origin: ContributionOrigin,
}

impl Unit {
    /// Returns the source unit identity.
    #[must_use]
    pub const fn source(&self) -> &SourceUnitId {
        &self.source
    }

    /// Returns where the unit's facts came from.
    #[must_use]
    pub const fn origin(&self) -> &ContributionOrigin {
        &self.origin
    }
}

/// A validated, immutable binding graph in insertion order.
#[derive(Debug, Clone)]
pub struct BindingGraph {
    units: Vec<Unit>,
    scopes: Vec<Scope>,
    definitions: Vec<Definition>,
    references: Vec<Reference>,
    links: Vec<Link>,
    definitions_by_name: BTreeMap<(ScopeId, Name), Vec<DefinitionId>>,
    links_by_scope: Vec<Vec<LinkId>>,
}

impl BindingGraph {
    /// Returns the unit table.
    #[must_use]
    pub fn units(&self) -> &[Unit] {
        &self.units
    }

    /// Returns every scope.
    #[must_use]
    pub fn scopes(&self) -> &[Scope] {
        &self.scopes
    }

    /// Returns every definition.
    #[must_use]
    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }

    /// Returns every reference.
    #[must_use]
    pub fn references(&self) -> &[Reference] {
        &self.references
    }

    /// Returns every link.
    #[must_use]
    pub fn links(&self) -> &[Link] {
        &self.links
    }

    /// Returns one unit.
    #[must_use]
    pub fn unit(&self, id: UnitId) -> &Unit {
        &self.units[id.index()]
    }

    /// Returns one scope.
    #[must_use]
    pub fn scope(&self, id: ScopeId) -> &Scope {
        &self.scopes[id.index()]
    }

    /// Returns one definition.
    #[must_use]
    pub fn definition(&self, id: DefinitionId) -> &Definition {
        &self.definitions[id.index()]
    }

    /// Returns one reference.
    #[must_use]
    pub fn reference(&self, id: ReferenceId) -> &Reference {
        &self.references[id.index()]
    }

    /// Returns one link.
    #[must_use]
    pub fn link(&self, id: LinkId) -> &Link {
        &self.links[id.index()]
    }

    /// Iterates unit ids in insertion order.
    pub fn unit_ids(&self) -> impl Iterator<Item = UnitId> + '_ {
        (0..self.units.len()).filter_map(UnitId::from_index)
    }

    /// Iterates scope ids in insertion order.
    pub fn scope_ids(&self) -> impl Iterator<Item = ScopeId> + '_ {
        (0..self.scopes.len()).filter_map(ScopeId::from_index)
    }

    /// Iterates definition ids in insertion order.
    pub fn definition_ids(&self) -> impl Iterator<Item = DefinitionId> + '_ {
        (0..self.definitions.len()).filter_map(DefinitionId::from_index)
    }

    /// Iterates reference ids in insertion order.
    pub fn reference_ids(&self) -> impl Iterator<Item = ReferenceId> + '_ {
        (0..self.references.len()).filter_map(ReferenceId::from_index)
    }

    /// Iterates link ids in insertion order.
    pub fn link_ids(&self) -> impl Iterator<Item = LinkId> + '_ {
        (0..self.links.len()).filter_map(LinkId::from_index)
    }

    /// Returns the definitions named `name` directly in `scope`, in insertion order.
    #[must_use]
    pub fn definitions_named(&self, scope: ScopeId, name: &Name) -> &[DefinitionId] {
        self.definitions_by_name
            .get(&(scope, name.clone()))
            .map_or(&[], Vec::as_slice)
    }

    /// Returns the links hanging from `scope`, in insertion order.
    #[must_use]
    pub fn links_at(&self, scope: ScopeId) -> &[LinkId] {
        &self.links_by_scope[scope.index()]
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct UnitCounts {
    scopes: usize,
    definitions: usize,
    references: usize,
    links: usize,
}

/// Collects one publication's binding facts and validates them into a [`BindingGraph`].
///
/// Every count is checked as the fact arrives, so no table outgrows its bound; scope
/// parents may name scopes added later and are validated by [`GraphBuilder::build`].
#[derive(Debug)]
pub struct GraphBuilder {
    limits: BindingLimits,
    units: Vec<Unit>,
    counts: Vec<UnitCounts>,
    scopes: Vec<Scope>,
    definitions: Vec<Definition>,
    references: Vec<Reference>,
    links: Vec<Link>,
}

impl GraphBuilder {
    /// Starts an empty graph under `limits`.
    #[must_use]
    pub fn new(limits: BindingLimits) -> Self {
        Self {
            limits,
            units: Vec::new(),
            counts: Vec::new(),
            scopes: Vec::new(),
            definitions: Vec::new(),
            references: Vec::new(),
            links: Vec::new(),
        }
    }

    /// Adds one unit to the unit table.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::GraphLimit`] when the unit table would exceed
    /// `graph_nodes_max`.
    pub fn unit(
        &mut self,
        source: SourceUnitId,
        origin: ContributionOrigin,
    ) -> Result<UnitId, BindingError> {
        let violation = BindingViolation::GraphLimit(ExhaustedLimit::GraphNodes);
        accept(self.units.len(), self.limits.graph_nodes_max(), violation)?;
        let id = minted(UnitId::from_index, self.units.len(), violation)?;
        self.units.push(Unit { source, origin });
        self.counts.push(UnitCounts::default());
        Ok(id)
    }

    /// Adds one scope.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::MissingUnit`] for an unknown unit, or the
    /// `unit_scopes_max` or `graph_nodes_max` limit violation.
    pub fn scope(&mut self, scope: Scope) -> Result<ScopeId, BindingError> {
        let unit = self.existing_unit(scope.unit())?;
        let violation = BindingViolation::UnitLimit(ExhaustedLimit::UnitScopes);
        accept(
            self.counts[unit].scopes,
            self.limits.unit_scopes_max(),
            violation,
        )?;
        self.accept_node()?;
        let id = minted(ScopeId::from_index, self.scopes.len(), node_limit())?;
        self.counts[unit].scopes += 1;
        self.scopes.push(scope);
        Ok(id)
    }

    /// Adds one definition to its scope.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::MissingScope`] for an unknown scope, or the
    /// `unit_definitions_max` or `graph_nodes_max` limit violation.
    pub fn definition(&mut self, definition: Definition) -> Result<DefinitionId, BindingError> {
        let unit = self.existing_scope(definition.scope())?.unit().index();
        if let Some(declared) = definition.declares() {
            self.existing_scope(declared)?;
        }
        let violation = BindingViolation::UnitLimit(ExhaustedLimit::UnitDefinitions);
        let count = self.counts[unit].definitions;
        accept(count, self.limits.unit_definitions_max(), violation)?;
        self.accept_node()?;
        let id = minted(
            DefinitionId::from_index,
            self.definitions.len(),
            node_limit(),
        )?;
        self.counts[unit].definitions += 1;
        self.definitions.push(definition);
        Ok(id)
    }

    /// Adds one reference to its scope.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::MissingScope`] for an unknown scope, or the
    /// `unit_references_max` or `graph_nodes_max` limit violation.
    pub fn reference(&mut self, reference: Reference) -> Result<ReferenceId, BindingError> {
        let unit = self.existing_scope(reference.scope())?.unit().index();
        let violation = BindingViolation::UnitLimit(ExhaustedLimit::UnitReferences);
        let count = self.counts[unit].references;
        accept(count, self.limits.unit_references_max(), violation)?;
        self.accept_node()?;
        let id = minted(ReferenceId::from_index, self.references.len(), node_limit())?;
        self.counts[unit].references += 1;
        self.references.push(reference);
        Ok(id)
    }

    /// Adds one link to its scope.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::MissingScope`] or [`BindingViolation::MissingDefinition`]
    /// for an unknown id, [`BindingViolation::MemberScopeKind`] when a member link names a
    /// scope that is not a member scope, or the `unit_links_max` or `graph_links_max` limit
    /// violation.
    pub fn link(&mut self, link: Link) -> Result<LinkId, BindingError> {
        let unit = self.existing_scope(link.scope())?.unit().index();
        self.validate_link_kind(link.kind())?;
        let violation = BindingViolation::UnitLimit(ExhaustedLimit::UnitLinks);
        accept(
            self.counts[unit].links,
            self.limits.unit_links_max(),
            violation,
        )?;
        let violation = BindingViolation::GraphLimit(ExhaustedLimit::GraphLinks);
        accept(self.links.len(), self.limits.graph_links_max(), violation)?;
        let id = minted(LinkId::from_index, self.links.len(), violation)?;
        self.counts[unit].links += 1;
        self.links.push(link);
        Ok(id)
    }

    /// Validates scope parents and freezes the graph.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::MissingScope`] for a parent that does not exist,
    /// [`BindingViolation::ScopeUnitMismatch`] for a parent in another unit, or
    /// [`BindingViolation::ScopeCycle`] when parents repeat.
    pub fn build(self) -> Result<BindingGraph, BindingError> {
        validate_parents(&self.scopes)?;
        let definitions_by_name = index_definitions(&self.definitions);
        let links_by_scope = index_links(&self.links, self.scopes.len());
        Ok(BindingGraph {
            units: self.units,
            scopes: self.scopes,
            definitions: self.definitions,
            references: self.references,
            links: self.links,
            definitions_by_name,
            links_by_scope,
        })
    }

    fn existing_unit(&self, unit: UnitId) -> Result<usize, BindingError> {
        if unit.index() < self.units.len() {
            return Ok(unit.index());
        }
        Err(binding_error(
            BindingViolation::MissingUnit,
            format!("{unit:?} is not in the unit table"),
        ))
    }

    fn existing_scope(&self, scope: ScopeId) -> Result<&Scope, BindingError> {
        self.scopes.get(scope.index()).ok_or_else(|| {
            binding_error(
                BindingViolation::MissingScope,
                format!("{scope:?} has not been added"),
            )
        })
    }

    fn validate_link_kind(&self, kind: &LinkKind) -> Result<(), BindingError> {
        match kind {
            LinkKind::Import { .. } => Ok(()),
            LinkKind::Member { scope, .. } => {
                let member = self.existing_scope(*scope)?;
                if member.kind() == ScopeKind::Member {
                    return Ok(());
                }
                let detail = format!("{scope:?} is a {:?} scope", member.kind());
                Err(binding_error(BindingViolation::MemberScopeKind, detail))
            }
            LinkKind::Module { definition, .. } => {
                if definition.index() < self.definitions.len() {
                    return Ok(());
                }
                let detail = format!("{definition:?} has not been added");
                Err(binding_error(BindingViolation::MissingDefinition, detail))
            }
        }
    }

    fn accept_node(&self) -> Result<(), BindingError> {
        let nodes = self.scopes.len() + self.definitions.len() + self.references.len();
        accept(nodes, self.limits.graph_nodes_max(), node_limit())
    }
}

const fn node_limit() -> BindingViolation {
    BindingViolation::GraphLimit(ExhaustedLimit::GraphNodes)
}

fn accept(count: usize, max: usize, violation: BindingViolation) -> Result<(), BindingError> {
    if count < max {
        return Ok(());
    }
    Err(binding_error(
        violation,
        format!("{count} already held, bound {max}"),
    ))
}

fn minted<Id>(
    mint: fn(usize) -> Option<Id>,
    index: usize,
    violation: BindingViolation,
) -> Result<Id, BindingError> {
    mint(index)
        .ok_or_else(|| binding_error(violation, format!("index {index} exceeds the id space")))
}

/// Walks every parent chain once; each scope enters `trail` exactly once, so the pass is
/// linear in the scope count and a chain longer than that count is a cycle.
fn validate_parents(scopes: &[Scope]) -> Result<(), BindingError> {
    let mut done = vec![false; scopes.len()];
    let mut trail = Vec::new();
    for start in 0..scopes.len() {
        if done[start] {
            continue;
        }
        walk_parent_chain(scopes, start, &done, &mut trail)?;
        for index in trail.drain(..) {
            done[index] = true;
        }
    }
    Ok(())
}

fn walk_parent_chain(
    scopes: &[Scope],
    start: usize,
    done: &[bool],
    trail: &mut Vec<usize>,
) -> Result<(), BindingError> {
    let mut budget = LoopBudget::new(scopes.len());
    let mut current = Some(start);
    while let Some(index) = current {
        if done[index] {
            return Ok(());
        }
        budget.consume().map_err(|_| {
            let detail = format!("the parent chain above scope {start} repeats");
            binding_error(BindingViolation::ScopeCycle, detail)
        })?;
        trail.push(index);
        current = parent_index(scopes, index)?;
    }
    Ok(())
}

fn parent_index(scopes: &[Scope], index: usize) -> Result<Option<usize>, BindingError> {
    let scope = &scopes[index];
    let Some(parent) = scope.parent() else {
        return Ok(None);
    };
    let parent_scope = scopes.get(parent.index()).ok_or_else(|| {
        let detail = format!("scope {index} names parent {parent:?}, which does not exist");
        binding_error(BindingViolation::MissingScope, detail)
    })?;
    if parent_scope.unit() != scope.unit() {
        let detail = format!("scope {index} and parent {parent:?} lie in different units");
        return Err(binding_error(BindingViolation::ScopeUnitMismatch, detail));
    }
    Ok(Some(parent.index()))
}

fn index_definitions(definitions: &[Definition]) -> BTreeMap<(ScopeId, Name), Vec<DefinitionId>> {
    let mut by_name: BTreeMap<(ScopeId, Name), Vec<DefinitionId>> = BTreeMap::new();
    let ids = (0..definitions.len()).filter_map(DefinitionId::from_index);
    for (definition, id) in definitions.iter().zip(ids) {
        by_name
            .entry((definition.scope(), definition.name().clone()))
            .or_default()
            .push(id);
    }
    by_name
}

fn index_links(links: &[Link], scope_count: usize) -> Vec<Vec<LinkId>> {
    let mut by_scope = vec![Vec::new(); scope_count];
    let ids = (0..links.len()).filter_map(LinkId::from_index);
    for (link, id) in links.iter().zip(ids) {
        by_scope[link.scope().index()].push(id);
    }
    by_scope
}

#[cfg(test)]
mod tests {
    use rift_core::{ReferenceRole, SymbolFacet};

    use super::{
        DefinitionId, DefinitionOrder, Link, LinkKind, NAME_BYTES_MAX, NAME_PATH_SEGMENTS_MAX,
        Name, NamePath, PathAnchor, Rank, Reference, Scope, ScopeId, ScopeKind, UnitId,
        VisibilitySpelling,
    };
    use crate::failure::{BindingError, BindingViolation};
    use crate::fixture::{Fixture, kind, name, origin, path, range, source};
    use crate::limits::{BindingLimits, ExhaustedLimit};

    fn violation<T>(result: Result<T, BindingError>) -> Option<BindingViolation> {
        result.err().map(|error| error.fault().violation())
    }

    #[test]
    fn test_name_new_empty_refused() {
        assert_eq!(
            violation(Name::new("")),
            Some(BindingViolation::InvalidName)
        );
    }

    #[test]
    fn test_name_new_over_bytes_max_refused() {
        let text = "x".repeat(NAME_BYTES_MAX + 1);
        assert_eq!(
            violation(Name::new(text)),
            Some(BindingViolation::InvalidName)
        );
    }

    #[test]
    fn test_name_new_at_bytes_max_accepted() {
        let text = "x".repeat(NAME_BYTES_MAX);
        let accepted = Name::new(text.clone()).ok();
        assert_eq!(accepted.as_ref().map(Name::as_str), Some(text.as_str()));
        assert_eq!(accepted.map(|name| name.to_string()), Some(text));
    }

    #[test]
    fn test_name_path_new_empty_refused() {
        assert_eq!(
            violation(NamePath::new(Vec::new())),
            Some(BindingViolation::InvalidPath)
        );
    }

    #[test]
    fn test_name_path_new_over_segments_max_refused() {
        let segments = (0..=NAME_PATH_SEGMENTS_MAX).map(|_| name("a")).collect();
        assert_eq!(
            violation(NamePath::new(segments)),
            Some(BindingViolation::InvalidPath)
        );
    }

    #[test]
    fn test_name_path_single_holds_one_segment() {
        let single = NamePath::single(name("f"));
        assert_eq!(single.segments(), &[name("f")]);
        assert_eq!(path("a::b").segments().len(), 2);
    }

    #[test]
    fn test_builder_definition_missing_scope_refused() {
        let mut fixture = Fixture::new();
        fixture.unit("src/lib.rs");
        let definition = super::Definition::new(
            ScopeId(7),
            name("f"),
            range(0, 1),
            kind(),
            DefinitionOrder::Item,
            VisibilitySpelling::Public,
        );
        let refused = fixture.builder.definition(definition);
        assert_eq!(violation(refused), Some(BindingViolation::MissingScope));
    }

    #[test]
    fn test_builder_definition_missing_declared_scope_refused() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let definition = super::Definition::new(
            root,
            name("m"),
            range(0, 1),
            kind(),
            DefinitionOrder::Item,
            VisibilitySpelling::Public,
        )
        .declaring(ScopeId(9));
        let refused = fixture.builder.definition(definition);
        assert_eq!(violation(refused), Some(BindingViolation::MissingScope));
    }

    #[test]
    fn test_builder_reference_missing_scope_refused() {
        let mut fixture = Fixture::new();
        let reference = Reference::new(
            ScopeId(0),
            range(0, 1),
            PathAnchor::Lexical,
            path("f"),
            ReferenceRole::Read,
        );
        let refused = fixture.builder.reference(reference);
        assert_eq!(violation(refused), Some(BindingViolation::MissingScope));
    }

    #[test]
    fn test_builder_scope_missing_unit_refused() {
        let mut fixture = Fixture::new();
        let scope = Scope::new(UnitId(3), range(0, 1), ScopeKind::Module, None);
        let refused = fixture.builder.scope(scope);
        assert_eq!(violation(refused), Some(BindingViolation::MissingUnit));
    }

    #[test]
    fn test_builder_build_scope_cycle_refused() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let first = fixture.block(unit, Some(ScopeId(1)), 0, 10);
        let second = fixture.block(unit, Some(first), 0, 10);
        assert_eq!(second, ScopeId(1));
        assert_eq!(
            violation(fixture.builder.build()),
            Some(BindingViolation::ScopeCycle)
        );
    }

    #[test]
    fn test_builder_build_missing_parent_refused() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        fixture.block(unit, Some(ScopeId(5)), 0, 10);
        assert_eq!(
            violation(fixture.builder.build()),
            Some(BindingViolation::MissingScope)
        );
    }

    #[test]
    fn test_builder_build_parent_in_other_unit_refused() {
        let mut fixture = Fixture::new();
        let first = fixture.unit("src/lib.rs");
        let second = fixture.unit("src/other.rs");
        let root = fixture.module(first, None, 0, 100);
        fixture.block(second, Some(root), 0, 10);
        let refused = fixture.builder.build();
        assert_eq!(
            violation(refused),
            Some(BindingViolation::ScopeUnitMismatch)
        );
    }

    #[test]
    fn test_builder_build_deep_chain_and_shared_parents_accepted() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let mut parent = root;
        for depth in 0..8 {
            parent = fixture.block(unit, Some(parent), depth, depth + 20);
        }
        fixture.block(unit, Some(root), 50, 60);
        fixture.block(unit, Some(parent), 51, 52);
        let graph = fixture.build();
        assert_eq!(graph.scopes().len(), 11);
        assert_eq!(graph.links_at(root), &[]);
    }

    #[test]
    fn test_builder_member_link_non_member_scope_refused() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let block = fixture.block(unit, Some(root), 10, 20);
        let link = Link::new(
            root,
            LinkKind::Member {
                owner_anchor: PathAnchor::Lexical,
                owner: path("T"),
                scope: block,
            },
            Rank::new(0),
        );
        let refused = fixture.builder.link(link);
        assert_eq!(violation(refused), Some(BindingViolation::MemberScopeKind));
    }

    #[test]
    fn test_builder_member_link_missing_scope_refused() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let link = Link::new(
            root,
            LinkKind::Member {
                owner_anchor: PathAnchor::Lexical,
                owner: path("T"),
                scope: ScopeId(4),
            },
            Rank::new(0),
        );
        let refused = fixture.builder.link(link);
        assert_eq!(violation(refused), Some(BindingViolation::MissingScope));
    }

    #[test]
    fn test_builder_link_missing_scope_refused() {
        let mut fixture = Fixture::new();
        let link = Link::new(
            ScopeId(0),
            LinkKind::Module {
                definition: DefinitionId(0),
                unit: source("src/a.rs"),
            },
            Rank::new(0),
        );
        let refused = fixture.builder.link(link);
        assert_eq!(violation(refused), Some(BindingViolation::MissingScope));
    }

    #[test]
    fn test_builder_module_link_missing_definition_refused() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let link = Link::new(
            root,
            LinkKind::Module {
                definition: DefinitionId(2),
                unit: source("src/a.rs"),
            },
            Rank::new(0),
        );
        let refused = fixture.builder.link(link);
        assert_eq!(
            violation(refused),
            Some(BindingViolation::MissingDefinition)
        );
    }

    #[test]
    fn test_builder_unit_scopes_at_limit_accepted_one_over_refused() {
        let limits = BindingLimits::builder().unit_scopes_max(2).build();
        let mut fixture = Fixture::with_limits(limits.unwrap_or_default());
        let unit = fixture.unit("src/lib.rs");
        let other = fixture.unit("src/other.rs");
        let root = fixture.module(unit, None, 0, 100);
        fixture.block(unit, Some(root), 1, 2);
        fixture.module(other, None, 0, 100);
        let refused = fixture
            .builder
            .scope(Scope::new(unit, range(3, 4), ScopeKind::Block, None));
        let expected = BindingViolation::UnitLimit(ExhaustedLimit::UnitScopes);
        assert_eq!(violation(refused), Some(expected));
    }

    #[test]
    fn test_builder_unit_definitions_at_limit_accepted_one_over_refused() {
        let limits = BindingLimits::builder().unit_definitions_max(1).build();
        let mut fixture = Fixture::with_limits(limits.unwrap_or_default());
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        fixture.item(root, "a", 0, 1, VisibilitySpelling::Public);
        let definition = super::Definition::new(
            root,
            name("b"),
            range(2, 3),
            kind(),
            DefinitionOrder::Item,
            VisibilitySpelling::Public,
        );
        let refused = fixture.builder.definition(definition);
        let expected = BindingViolation::UnitLimit(ExhaustedLimit::UnitDefinitions);
        assert_eq!(violation(refused), Some(expected));
    }

    #[test]
    fn test_builder_unit_references_at_limit_accepted_one_over_refused() {
        let limits = BindingLimits::builder().unit_references_max(1).build();
        let mut fixture = Fixture::with_limits(limits.unwrap_or_default());
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        fixture.reference(root, 5, PathAnchor::Lexical, "a");
        let reference = Reference::new(
            root,
            range(6, 7),
            PathAnchor::Lexical,
            path("b"),
            ReferenceRole::Read,
        );
        let refused = fixture.builder.reference(reference);
        let expected = BindingViolation::UnitLimit(ExhaustedLimit::UnitReferences);
        assert_eq!(violation(refused), Some(expected));
    }

    #[test]
    fn test_builder_unit_links_at_limit_accepted_one_over_refused() {
        let limits = BindingLimits::builder().unit_links_max(1).build();
        let mut fixture = Fixture::with_limits(limits.unwrap_or_default());
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        fixture.import(
            root,
            Some("a"),
            PathAnchor::SelfModule,
            "m::a",
            0,
            VisibilitySpelling::Private,
        );
        let link = Link::new(
            root,
            LinkKind::Import {
                name: None,
                anchor: PathAnchor::SelfModule,
                path: path("m"),
                visibility: VisibilitySpelling::Private,
            },
            Rank::new(1),
        );
        let refused = fixture.builder.link(link);
        let expected = BindingViolation::UnitLimit(ExhaustedLimit::UnitLinks);
        assert_eq!(violation(refused), Some(expected));
    }

    #[test]
    fn test_builder_graph_nodes_at_limit_accepted_one_over_refused() {
        let limits = BindingLimits::builder().graph_nodes_max(3).build();
        let mut fixture = Fixture::with_limits(limits.unwrap_or_default());
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        fixture.item(root, "a", 0, 1, VisibilitySpelling::Public);
        fixture.reference(root, 5, PathAnchor::Lexical, "a");
        let refused = fixture
            .builder
            .scope(Scope::new(unit, range(3, 4), ScopeKind::Block, None));
        let expected = BindingViolation::GraphLimit(ExhaustedLimit::GraphNodes);
        assert_eq!(violation(refused), Some(expected));
    }

    #[test]
    fn test_builder_units_over_graph_nodes_refused() {
        let limits = BindingLimits::builder().graph_nodes_max(1).build();
        let mut fixture = Fixture::with_limits(limits.unwrap_or_default());
        fixture.unit("src/lib.rs");
        let refused = fixture.builder.unit(source("src/other.rs"), origin());
        let expected = BindingViolation::GraphLimit(ExhaustedLimit::GraphNodes);
        assert_eq!(violation(refused), Some(expected));
    }

    #[test]
    fn test_builder_graph_links_at_limit_accepted_one_over_refused() {
        let limits = BindingLimits::builder().graph_links_max(1).build();
        let mut fixture = Fixture::with_limits(limits.unwrap_or_default());
        let unit = fixture.unit("src/lib.rs");
        let other = fixture.unit("src/other.rs");
        let root = fixture.module(unit, None, 0, 100);
        let other_root = fixture.module(other, None, 0, 100);
        fixture.import(
            root,
            Some("a"),
            PathAnchor::SelfModule,
            "m::a",
            0,
            VisibilitySpelling::Private,
        );
        let link = Link::new(
            other_root,
            LinkKind::Import {
                name: Some(name("b")),
                anchor: PathAnchor::SelfModule,
                path: path("m::b"),
                visibility: VisibilitySpelling::Private,
            },
            Rank::new(0),
        );
        let refused = fixture.builder.link(link);
        let expected = BindingViolation::GraphLimit(ExhaustedLimit::GraphLinks);
        assert_eq!(violation(refused), Some(expected));
    }

    #[test]
    fn test_graph_accessors_expose_insertion_order_and_indexes() {
        let mut fixture = Fixture::new();
        let unit = fixture.unit("src/lib.rs");
        let root = fixture.module(unit, None, 0, 100);
        let body = fixture.member(unit, Some(root), 10, 40);
        let first = fixture.item(root, "f", 0, 5, VisibilitySpelling::Public);
        let second = fixture.item(root, "f", 6, 9, VisibilitySpelling::Private);
        let definition = super::Definition::new(
            root,
            name("T"),
            range(10, 40),
            kind(),
            DefinitionOrder::Sequential(40),
            VisibilitySpelling::Crate,
        )
        .with_facets(vec![SymbolFacet::Type])
        .declaring(body)
        .item();
        let typed = fixture.builder.definition(definition).unwrap_or(first);
        let reference = fixture.reference(root, 50, PathAnchor::Crate, "T::new");
        let link = fixture.import(
            root,
            None,
            PathAnchor::Super(1),
            "m",
            1,
            VisibilitySpelling::Super,
        );
        let graph = fixture.build();

        assert_eq!(graph.units().len(), 1);
        assert_eq!(graph.unit(unit).source(), &source("src/lib.rs"));
        assert_eq!(graph.unit(unit).origin(), &origin());
        assert_eq!(graph.scopes().len(), 2);
        assert_eq!(graph.scope(body).kind(), ScopeKind::Member);
        assert_eq!(graph.scope(body).parent(), Some(root));
        assert_eq!(graph.scope(body).range(), range(10, 40));
        assert_eq!(graph.scope(body).unit(), unit);
        assert_eq!(graph.definitions().len(), 3);
        assert_eq!(graph.definitions_named(root, &name("f")), &[first, second]);
        assert_eq!(graph.definitions_named(body, &name("f")), &[]);
        let stored = graph.definition(typed);
        assert_eq!(stored.name().as_str(), "T");
        assert_eq!(stored.scope(), root);
        assert_eq!(stored.range(), range(10, 40));
        assert_eq!(stored.kind(), &kind());
        assert_eq!(stored.facets(), &[SymbolFacet::Type]);
        assert_eq!(stored.order(), DefinitionOrder::Sequential(40));
        assert_eq!(stored.visibility(), VisibilitySpelling::Crate);
        assert_eq!(stored.declares(), Some(body));
        assert!(stored.is_item());
        assert!(!graph.definition(first).is_item());
        assert_eq!(graph.references().len(), 1);
        let stored = graph.reference(reference);
        assert_eq!(stored.scope(), root);
        assert_eq!(stored.range(), range(50, 51));
        assert_eq!(stored.anchor(), PathAnchor::Crate);
        assert_eq!(stored.path(), &path("T::new"));
        assert_eq!(stored.role(), ReferenceRole::Read);
        assert_eq!(graph.links().len(), 1);
        assert_eq!(graph.link(link).scope(), root);
        assert_eq!(graph.link(link).rank(), Rank::new(1));
        assert_eq!(graph.link(link).rank().value(), 1);
        assert!(matches!(
            graph.link(link).kind(),
            LinkKind::Import { name: None, .. }
        ));
        assert_eq!(graph.links_at(root), &[link]);
        assert_eq!(graph.links_at(body), &[]);
        assert_eq!(graph.scope_ids().collect::<Vec<_>>(), vec![root, body]);
        assert_eq!(graph.unit_ids().collect::<Vec<_>>(), vec![unit]);
        assert_eq!(path("a::b").head(), &name("a"));
        assert_eq!(path("a::b").tail(), &[name("b")]);
        assert_eq!(graph.definition_ids().count(), 3);
        assert_eq!(graph.reference_ids().collect::<Vec<_>>(), vec![reference]);
        assert_eq!(graph.link_ids().collect::<Vec<_>>(), vec![link]);
        assert_eq!(graph.clone().scopes().len(), 2);
    }
}
