//! One unit's binding facts under unit-local indices, and their assembly into one graph.
//!
//! A language rule builds [`UnitBindingFacts`] per source unit through
//! [`UnitBindingFactsBuilder`], which validates indices and per-unit bounds as each fact
//! arrives. [`assemble`] maps every unit's local indices to graph ids through
//! [`GraphBuilder`] and resolves each module declaration's candidate paths against the
//! supplied unit set.

use std::collections::BTreeMap;

use rift_core::{
    ContributionOrigin, ExactKind, ReferenceRole, SourceRange, SourceUnitId, SymbolFacet,
    encode_path,
};

use crate::failure::{BindingError, BindingViolation, binding_error};
use crate::graph::{
    BindingGraph, Definition, DefinitionOrder, GraphBuilder, Link, LinkKind, Name, NamePath,
    PathAnchor, Rank, Reference, Scope, ScopeKind, VisibilitySpelling,
};
use crate::limits::{BindingLimits, ExhaustedLimit};

/// Rank of an explicit import link.
pub const IMPORT_EXPLICIT_RANK: Rank = Rank::new(0);
/// Rank of a wildcard import link.
pub const IMPORT_WILDCARD_RANK: Rank = Rank::new(1);
/// Rank of a member link; the linking phase resolves owners outside path competition.
const MEMBER_LINK_RANK: Rank = Rank::new(0);
/// Rank of a module link; module links join units outside path competition.
const MODULE_LINK_RANK: Rank = Rank::new(0);
/// URI prefix an absent module candidate is minted under, matching project source units.
const PROJECT_UNIT_URI_PREFIX: &str = "rift://source/project/";

macro_rules! define_unit_index {
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

define_unit_index!(
    UnitScopeIndex,
    "Position of one scope in its unit's scope table."
);
define_unit_index!(
    UnitDefinitionIndex,
    "Position of one definition in its unit's definition table."
);

/// One scope of a unit, with its lexical parent as a unit-local index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitScope {
    kind: ScopeKind,
    range: SourceRange,
    parent: Option<UnitScopeIndex>,
}

impl UnitScope {
    /// Returns the scope kind.
    #[must_use]
    pub const fn kind(&self) -> ScopeKind {
        self.kind
    }

    /// Returns the scope's byte range.
    #[must_use]
    pub const fn range(&self) -> SourceRange {
        self.range
    }

    /// Returns the lexical parent, absent for the unit scope.
    #[must_use]
    pub const fn parent(&self) -> Option<UnitScopeIndex> {
        self.parent
    }
}

/// One named declaration of a unit, addressed by unit-local scope indices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitDefinition {
    scope: UnitScopeIndex,
    name: Name,
    range: SourceRange,
    kind: ExactKind,
    facets: Vec<SymbolFacet>,
    order: DefinitionOrder,
    visibility: VisibilitySpelling,
    declares: Option<UnitScopeIndex>,
    is_item: bool,
}

impl UnitDefinition {
    /// Describes one definition; `range` is the whole declaration item.
    #[must_use]
    pub const fn new(
        scope: UnitScopeIndex,
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
    pub const fn declaring(mut self, scope: UnitScopeIndex) -> Self {
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
    pub const fn scope(&self) -> UnitScopeIndex {
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
    pub const fn declares(&self) -> Option<UnitScopeIndex> {
        self.declares
    }

    /// Whether a syntax declaration shares this definition.
    #[must_use]
    pub const fn is_item(&self) -> bool {
        self.is_item
    }
}

/// One name-path occurrence of a unit, addressed by a unit-local scope index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitReference {
    scope: UnitScopeIndex,
    range: SourceRange,
    anchor: PathAnchor,
    path: NamePath,
    role: ReferenceRole,
}

impl UnitReference {
    /// Describes one reference occurring at `range` inside `scope`.
    #[must_use]
    pub const fn new(
        scope: UnitScopeIndex,
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
    pub const fn scope(&self) -> UnitScopeIndex {
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

/// One import of a unit: the alias or last segment it binds, or a wildcard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitImport {
    scope: UnitScopeIndex,
    name: Option<Name>,
    anchor: PathAnchor,
    path: NamePath,
    visibility: VisibilitySpelling,
    rank: Rank,
}

impl UnitImport {
    /// Describes one import at `scope`; a `name` of `None` is a wildcard.
    #[must_use]
    pub const fn new(
        scope: UnitScopeIndex,
        name: Option<Name>,
        anchor: PathAnchor,
        path: NamePath,
        visibility: VisibilitySpelling,
        rank: Rank,
    ) -> Self {
        Self {
            scope,
            name,
            anchor,
            path,
            visibility,
            rank,
        }
    }

    /// Returns the scope holding the import.
    #[must_use]
    pub const fn scope(&self) -> UnitScopeIndex {
        self.scope
    }

    /// Returns the bound alias or last segment; `None` for a wildcard.
    #[must_use]
    pub const fn name(&self) -> Option<&Name> {
        self.name.as_ref()
    }

    /// Returns where the imported path starts.
    #[must_use]
    pub const fn anchor(&self) -> PathAnchor {
        self.anchor
    }

    /// Returns the imported path.
    #[must_use]
    pub const fn path(&self) -> &NamePath {
        &self.path
    }

    /// Returns the spelled visibility.
    #[must_use]
    pub const fn visibility(&self) -> VisibilitySpelling {
        self.visibility
    }

    /// Returns the import's precedence.
    #[must_use]
    pub const fn rank(&self) -> Rank {
        self.rank
    }
}

/// One member link of a unit: a member scope and the owner path that names its holder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitMemberLink {
    scope: UnitScopeIndex,
    owner_anchor: PathAnchor,
    owner: NamePath,
    member: UnitScopeIndex,
}

impl UnitMemberLink {
    /// Attaches member scope `member` to the definitions `owner` names from `scope`.
    #[must_use]
    pub const fn new(
        scope: UnitScopeIndex,
        owner_anchor: PathAnchor,
        owner: NamePath,
        member: UnitScopeIndex,
    ) -> Self {
        Self {
            scope,
            owner_anchor,
            owner,
            member,
        }
    }

    /// Returns the scope the owner path is looked up from.
    #[must_use]
    pub const fn scope(&self) -> UnitScopeIndex {
        self.scope
    }

    /// Returns where the owner path starts.
    #[must_use]
    pub const fn owner_anchor(&self) -> PathAnchor {
        self.owner_anchor
    }

    /// Returns the path naming the owner definition.
    #[must_use]
    pub const fn owner(&self) -> &NamePath {
        &self.owner
    }

    /// Returns the member scope to attach.
    #[must_use]
    pub const fn member(&self) -> UnitScopeIndex {
        self.member
    }
}

/// One body-less module declaration and the unit paths that could hold its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitModuleDeclaration {
    definition: UnitDefinitionIndex,
    candidates: Vec<String>,
}

impl UnitModuleDeclaration {
    /// Pairs a `mod` definition with its candidate project paths, strongest first.
    #[must_use]
    pub const fn new(definition: UnitDefinitionIndex, candidates: Vec<String>) -> Self {
        Self {
            definition,
            candidates,
        }
    }

    /// Returns the `mod` definition.
    #[must_use]
    pub const fn definition(&self) -> UnitDefinitionIndex {
        self.definition
    }

    /// Returns the candidate project paths, strongest first.
    #[must_use]
    pub fn candidates(&self) -> &[String] {
        &self.candidates
    }
}

/// One unit's validated binding facts under unit-local indices.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UnitBindingFacts {
    scopes: Vec<UnitScope>,
    definitions: Vec<UnitDefinition>,
    references: Vec<UnitReference>,
    imports: Vec<UnitImport>,
    member_links: Vec<UnitMemberLink>,
    module_declarations: Vec<UnitModuleDeclaration>,
}

impl UnitBindingFacts {
    /// Starts an empty fact set bounded by `limits`.
    #[must_use]
    pub fn builder(limits: BindingLimits) -> UnitBindingFactsBuilder {
        UnitBindingFactsBuilder {
            limits,
            facts: Self::default(),
        }
    }

    /// Returns every scope.
    #[must_use]
    pub fn scopes(&self) -> &[UnitScope] {
        &self.scopes
    }

    /// Returns every definition.
    #[must_use]
    pub fn definitions(&self) -> &[UnitDefinition] {
        &self.definitions
    }

    /// Returns every reference.
    #[must_use]
    pub fn references(&self) -> &[UnitReference] {
        &self.references
    }

    /// Returns every import.
    #[must_use]
    pub fn imports(&self) -> &[UnitImport] {
        &self.imports
    }

    /// Returns every member link.
    #[must_use]
    pub fn member_links(&self) -> &[UnitMemberLink] {
        &self.member_links
    }

    /// Returns every module declaration.
    #[must_use]
    pub fn module_declarations(&self) -> &[UnitModuleDeclaration] {
        &self.module_declarations
    }

    /// Returns one scope by its unit-local index.
    #[must_use]
    pub fn scope(&self, index: UnitScopeIndex) -> Option<&UnitScope> {
        self.scopes.get(index.index())
    }

    /// Returns one definition by its unit-local index.
    #[must_use]
    pub fn definition(&self, index: UnitDefinitionIndex) -> Option<&UnitDefinition> {
        self.definitions.get(index.index())
    }

    /// The same facts with `declarations` replacing every module declaration.
    ///
    /// A layout pass recomputes candidate paths once the project path set is known; the
    /// replacement revalidates what the builder checked as each declaration arrived.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::MissingDefinition`] for an unknown definition,
    /// [`BindingViolation::InvalidPath`] for an empty candidate list, or the
    /// `unit_links_max` limit violation over the unit's imports, member links, and
    /// replaced declarations together.
    pub fn with_module_declarations(
        &self,
        declarations: Vec<UnitModuleDeclaration>,
        limits: &BindingLimits,
    ) -> Result<Self, BindingError> {
        for declaration in &declarations {
            self.accept_declaration(declaration)?;
        }
        let held = self.imports.len() + self.member_links.len() + declarations.len();
        if held > limits.unit_links_max() {
            return Err(binding_error(
                BindingViolation::UnitLimit(ExhaustedLimit::UnitLinks),
                format!("{held} would be held, bound {}", limits.unit_links_max()),
            ));
        }
        let mut replaced = self.clone();
        replaced.module_declarations = declarations;
        Ok(replaced)
    }

    /// Refuses a declaration naming an unknown definition or carrying no candidates.
    fn accept_declaration(&self, declaration: &UnitModuleDeclaration) -> Result<(), BindingError> {
        if declaration.definition().index() >= self.definitions.len() {
            let detail = format!("{:?} has not been added", declaration.definition());
            return Err(binding_error(BindingViolation::MissingDefinition, detail));
        }
        if declaration.candidates().is_empty() {
            return Err(binding_error(
                BindingViolation::InvalidPath,
                "a module declaration needs at least one candidate path",
            ));
        }
        Ok(())
    }
}

/// Collects one unit's facts, validating indices and per-unit bounds as each arrives.
///
/// A scope's parent must precede it, so unit-local indices always map onto graph ids in
/// one forward pass; imports, member links, and module declarations share the unit's
/// link bound.
#[derive(Debug)]
pub struct UnitBindingFactsBuilder {
    limits: BindingLimits,
    facts: UnitBindingFacts,
}

impl UnitBindingFactsBuilder {
    /// Adds one scope; `parent` names an already-added scope.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::MissingScope`] when the parent has not been added, or
    /// the `unit_scopes_max` limit violation.
    pub fn scope(
        &mut self,
        kind: ScopeKind,
        range: SourceRange,
        parent: Option<UnitScopeIndex>,
    ) -> Result<UnitScopeIndex, BindingError> {
        if let Some(parent) = parent {
            self.existing_scope(parent)?;
        }
        accept(
            self.facts.scopes.len(),
            self.limits.unit_scopes_max(),
            ExhaustedLimit::UnitScopes,
        )?;
        let index = minted(UnitScopeIndex::from_index, self.facts.scopes.len())?;
        self.facts.scopes.push(UnitScope {
            kind,
            range,
            parent,
        });
        Ok(index)
    }

    /// Adds one definition to its scope.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::MissingScope`] for an unknown scope or declared scope,
    /// or the `unit_definitions_max` limit violation.
    pub fn definition(
        &mut self,
        definition: UnitDefinition,
    ) -> Result<UnitDefinitionIndex, BindingError> {
        self.existing_scope(definition.scope())?;
        if let Some(declared) = definition.declares() {
            self.existing_scope(declared)?;
        }
        accept(
            self.facts.definitions.len(),
            self.limits.unit_definitions_max(),
            ExhaustedLimit::UnitDefinitions,
        )?;
        let index = minted(
            UnitDefinitionIndex::from_index,
            self.facts.definitions.len(),
        )?;
        self.facts.definitions.push(definition);
        Ok(index)
    }

    /// Adds one reference to its scope.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::MissingScope`] for an unknown scope, or the
    /// `unit_references_max` limit violation.
    pub fn reference(&mut self, reference: UnitReference) -> Result<(), BindingError> {
        self.existing_scope(reference.scope())?;
        accept(
            self.facts.references.len(),
            self.limits.unit_references_max(),
            ExhaustedLimit::UnitReferences,
        )?;
        self.facts.references.push(reference);
        Ok(())
    }

    /// Adds one import link to its scope.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::MissingScope`] for an unknown scope, or the
    /// `unit_links_max` limit violation.
    pub fn import(&mut self, import: UnitImport) -> Result<(), BindingError> {
        self.existing_scope(import.scope())?;
        self.accept_link()?;
        self.facts.imports.push(import);
        Ok(())
    }

    /// Adds one member link.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::MissingScope`] for an unknown scope,
    /// [`BindingViolation::MemberScopeKind`] when the member scope is not a member scope,
    /// or the `unit_links_max` limit violation.
    pub fn member_link(&mut self, link: UnitMemberLink) -> Result<(), BindingError> {
        self.existing_scope(link.scope())?;
        let member = self.existing_scope(link.member())?;
        if member.kind() != ScopeKind::Member {
            let detail = format!("{:?} is a {:?} scope", link.member(), member.kind());
            return Err(binding_error(BindingViolation::MemberScopeKind, detail));
        }
        self.accept_link()?;
        self.facts.member_links.push(link);
        Ok(())
    }

    /// Adds one module declaration.
    ///
    /// # Errors
    ///
    /// Returns [`BindingViolation::MissingDefinition`] for an unknown definition,
    /// [`BindingViolation::InvalidPath`] for an empty candidate list, or the
    /// `unit_links_max` limit violation.
    pub fn module_declaration(
        &mut self,
        declaration: UnitModuleDeclaration,
    ) -> Result<(), BindingError> {
        self.facts.accept_declaration(&declaration)?;
        self.accept_link()?;
        self.facts.module_declarations.push(declaration);
        Ok(())
    }

    /// Freezes the unit's facts.
    #[must_use]
    pub fn build(self) -> UnitBindingFacts {
        self.facts
    }

    fn existing_scope(&self, scope: UnitScopeIndex) -> Result<&UnitScope, BindingError> {
        self.facts.scopes.get(scope.index()).ok_or_else(|| {
            binding_error(
                BindingViolation::MissingScope,
                format!("{scope:?} has not been added"),
            )
        })
    }

    fn accept_link(&self) -> Result<(), BindingError> {
        let held = self.facts.imports.len()
            + self.facts.member_links.len()
            + self.facts.module_declarations.len();
        accept(
            held,
            self.limits.unit_links_max(),
            ExhaustedLimit::UnitLinks,
        )
    }
}

fn accept(count: usize, max: usize, limit: ExhaustedLimit) -> Result<(), BindingError> {
    if count < max {
        return Ok(());
    }
    Err(binding_error(
        BindingViolation::UnitLimit(limit),
        format!("{count} already held, bound {max}"),
    ))
}

fn minted<Index>(mint: fn(usize) -> Option<Index>, index: usize) -> Result<Index, BindingError> {
    mint(index).ok_or_else(|| {
        binding_error(
            BindingViolation::GraphLimit(ExhaustedLimit::GraphNodes),
            format!("index {index} exceeds the id space"),
        )
    })
}

/// Ids one unit's local indices map to.
struct UnitTables {
    scopes: Vec<crate::graph::ScopeId>,
    definitions: Vec<crate::graph::DefinitionId>,
}

/// Assembles many units' facts into one validated [`BindingGraph`].
///
/// Each module declaration's first candidate present in `units` becomes its module
/// link's target; with none present, the link targets the first candidate that forms a
/// valid project unit identity, so the linking phase records the module unlinked. The
/// loops run over tables the unit and graph builders already bounded.
///
/// # Errors
///
/// Returns [`BindingError`] when a graph bound is exceeded or an id does not validate.
pub fn assemble(
    units: &[(SourceUnitId, ContributionOrigin, &UnitBindingFacts)],
    limits: &BindingLimits,
) -> Result<BindingGraph, BindingError> {
    let mut builder = GraphBuilder::new(*limits);
    let present: BTreeMap<&str, &SourceUnitId> = units
        .iter()
        .map(|(source, ..)| (source.key().as_str(), source))
        .collect();
    for (source, origin, facts) in units {
        let tables = assemble_nodes(&mut builder, source, origin, facts)?;
        assemble_links(&mut builder, &tables, facts, &present)?;
    }
    builder.build()
}

fn assemble_nodes(
    builder: &mut GraphBuilder,
    source: &SourceUnitId,
    origin: &ContributionOrigin,
    facts: &UnitBindingFacts,
) -> Result<UnitTables, BindingError> {
    let unit = builder.unit(source.clone(), origin.clone())?;
    let mut scopes = Vec::with_capacity(facts.scopes().len());
    for scope in facts.scopes() {
        let parent = scope.parent().map(|parent| scopes[parent.index()]);
        let mapped = Scope::new(unit, scope.range(), scope.kind(), parent);
        scopes.push(builder.scope(mapped)?);
    }
    let mut definitions = Vec::with_capacity(facts.definitions().len());
    for definition in facts.definitions() {
        let mapped = mapped_definition(&scopes, definition);
        definitions.push(builder.definition(mapped)?);
    }
    for reference in facts.references() {
        let mapped = Reference::new(
            scopes[reference.scope().index()],
            reference.range(),
            reference.anchor(),
            reference.path().clone(),
            reference.role(),
        );
        builder.reference(mapped)?;
    }
    Ok(UnitTables {
        scopes,
        definitions,
    })
}

fn mapped_definition(scopes: &[crate::graph::ScopeId], definition: &UnitDefinition) -> Definition {
    let mut mapped = Definition::new(
        scopes[definition.scope().index()],
        definition.name().clone(),
        definition.range(),
        definition.kind().clone(),
        definition.order(),
        definition.visibility(),
    )
    .with_facets(definition.facets().to_vec());
    if let Some(declares) = definition.declares() {
        mapped = mapped.declaring(scopes[declares.index()]);
    }
    if definition.is_item() {
        mapped = mapped.item();
    }
    mapped
}

fn assemble_links(
    builder: &mut GraphBuilder,
    tables: &UnitTables,
    facts: &UnitBindingFacts,
    present: &BTreeMap<&str, &SourceUnitId>,
) -> Result<(), BindingError> {
    for import in facts.imports() {
        let kind = LinkKind::Import {
            name: import.name().cloned(),
            anchor: import.anchor(),
            path: import.path().clone(),
            visibility: import.visibility(),
        };
        let scope = tables.scopes[import.scope().index()];
        builder.link(Link::new(scope, kind, import.rank()))?;
    }
    for member in facts.member_links() {
        let kind = LinkKind::Member {
            owner_anchor: member.owner_anchor(),
            owner: member.owner().clone(),
            scope: tables.scopes[member.member().index()],
        };
        let scope = tables.scopes[member.scope().index()];
        builder.link(Link::new(scope, kind, MEMBER_LINK_RANK))?;
    }
    for declaration in facts.module_declarations() {
        let Some(unit) = declared_unit(declaration.candidates(), present) else {
            continue;
        };
        let definition = tables.definitions[declaration.definition().index()];
        let holder = facts.definitions()[declaration.definition().index()].scope();
        let scope = tables.scopes[holder.index()];
        let kind = LinkKind::Module { definition, unit };
        builder.link(Link::new(scope, kind, MODULE_LINK_RANK))?;
    }
    Ok(())
}

/// The first candidate the supplied unit set holds, else the first candidate that forms
/// a valid absent identity; `None` when no candidate forms one.
fn declared_unit(
    candidates: &[String],
    present: &BTreeMap<&str, &SourceUnitId>,
) -> Option<SourceUnitId> {
    for candidate in candidates {
        if let Some(unit) = present.get(candidate.as_str()) {
            return Some((*unit).clone());
        }
    }
    candidates.iter().find_map(|candidate| {
        let identity = format!("{PROJECT_UNIT_URI_PREFIX}{}", encode_path(candidate));
        SourceUnitId::parse(&identity).ok()
    })
}

#[cfg(test)]
mod tests {
    use rift_core::ReferenceRole;

    use super::{
        IMPORT_EXPLICIT_RANK, UnitBindingFacts, UnitBindingFactsBuilder, UnitDefinition,
        UnitDefinitionIndex, UnitImport, UnitMemberLink, UnitModuleDeclaration, UnitReference,
        UnitScopeIndex, assemble, minted,
    };
    use crate::failure::{BindingError, BindingViolation};
    use crate::fixture::{kind, name, origin, path, range, source};
    use crate::graph::{DefinitionOrder, LinkKind, PathAnchor, ScopeKind, VisibilitySpelling};
    use crate::limits::{BindingLimits, ExhaustedLimit};
    use crate::link::LinkedGraph;
    use crate::resolve::{NeverCancelled, resolve_all};

    fn violation<T>(result: Result<T, BindingError>) -> Option<BindingViolation> {
        result.err().map(|error| error.fault().violation())
    }

    fn builder() -> UnitBindingFactsBuilder {
        UnitBindingFacts::builder(BindingLimits::default())
    }

    fn definition(scope: UnitScopeIndex, text: &str, start: u64) -> UnitDefinition {
        UnitDefinition::new(
            scope,
            name(text),
            range(start, start + 5),
            kind(),
            DefinitionOrder::Item,
            VisibilitySpelling::Public,
        )
    }

    fn import(scope: UnitScopeIndex, alias: &str, target: &str) -> UnitImport {
        UnitImport::new(
            scope,
            Some(name(alias)),
            PathAnchor::SelfModule,
            path(target),
            VisibilitySpelling::Private,
            IMPORT_EXPLICIT_RANK,
        )
    }

    fn reference(scope: UnitScopeIndex, start: u64, target: &str) -> UnitReference {
        UnitReference::new(
            scope,
            range(start, start + 1),
            PathAnchor::Lexical,
            path(target),
            ReferenceRole::Read,
        )
    }

    /// A unit whose root module declares `mod x;` resolved through `candidates`.
    fn declaring_facts(candidates: &[&str]) -> UnitBindingFacts {
        let mut builder = builder();
        let root = builder
            .scope(ScopeKind::Module, range(0, 100), None)
            .expect("root scope accepted");
        let declaration = builder
            .definition(definition(root, "x", 0))
            .expect("definition accepted");
        builder
            .reference(reference(root, 50, "x::run"))
            .expect("reference accepted");
        let candidates = candidates.iter().map(ToString::to_string).collect();
        builder
            .module_declaration(UnitModuleDeclaration::new(declaration, candidates))
            .expect("module declaration accepted");
        builder.build()
    }

    /// A unit holding one public `run` definition at byte 10.
    fn run_facts() -> UnitBindingFacts {
        let mut builder = builder();
        let root = builder
            .scope(ScopeKind::Module, range(0, 100), None)
            .expect("root scope accepted");
        builder
            .definition(definition(root, "run", 10))
            .expect("definition accepted");
        builder.build()
    }

    #[test]
    fn test_unit_minted_index_beyond_id_space_refused() {
        let refused = minted(UnitDefinitionIndex::from_index, usize::MAX);
        assert_eq!(
            violation(refused),
            Some(BindingViolation::GraphLimit(ExhaustedLimit::GraphNodes))
        );
        assert!(
            minted(UnitDefinitionIndex::from_index, 0).is_ok(),
            "an index inside the id space mints"
        );
    }

    #[test]
    fn test_unit_builder_scope_parent_must_precede_child() {
        let mut builder = builder();
        let refused = builder.scope(ScopeKind::Block, range(0, 10), Some(UnitScopeIndex(0)));
        assert_eq!(violation(refused), Some(BindingViolation::MissingScope));
        let root = builder
            .scope(ScopeKind::Module, range(0, 100), None)
            .expect("root scope accepted");
        let child = builder.scope(ScopeKind::Block, range(1, 9), Some(root));
        assert!(child.is_ok(), "an added parent is accepted");
    }

    #[test]
    fn test_unit_builder_scopes_one_over_limit_refused() {
        let limits = BindingLimits::builder().unit_scopes_max(1).build();
        let mut builder = UnitBindingFacts::builder(limits.unwrap_or_default());
        builder
            .scope(ScopeKind::Module, range(0, 100), None)
            .expect("first scope accepted");
        let refused = builder.scope(ScopeKind::Block, range(1, 9), None);
        let expected = BindingViolation::UnitLimit(ExhaustedLimit::UnitScopes);
        assert_eq!(violation(refused), Some(expected));
    }

    #[test]
    fn test_unit_builder_definition_unknown_scope_refused() {
        let mut builder = builder();
        let refused = builder.definition(definition(UnitScopeIndex(3), "f", 0));
        assert_eq!(violation(refused), Some(BindingViolation::MissingScope));
    }

    #[test]
    fn test_unit_builder_definition_unknown_declared_scope_refused() {
        let mut builder = builder();
        let root = builder
            .scope(ScopeKind::Module, range(0, 100), None)
            .expect("root scope accepted");
        let refused = builder.definition(definition(root, "m", 0).declaring(UnitScopeIndex(9)));
        assert_eq!(violation(refused), Some(BindingViolation::MissingScope));
    }

    #[test]
    fn test_unit_builder_definitions_one_over_limit_refused() {
        let limits = BindingLimits::builder().unit_definitions_max(1).build();
        let mut builder = UnitBindingFacts::builder(limits.unwrap_or_default());
        let root = builder
            .scope(ScopeKind::Module, range(0, 100), None)
            .expect("root scope accepted");
        builder
            .definition(definition(root, "a", 0))
            .expect("first definition accepted");
        let refused = builder.definition(definition(root, "b", 6));
        let expected = BindingViolation::UnitLimit(ExhaustedLimit::UnitDefinitions);
        assert_eq!(violation(refused), Some(expected));
    }

    #[test]
    fn test_unit_builder_reference_unknown_scope_refused() {
        let mut builder = builder();
        let refused = builder.reference(reference(UnitScopeIndex(0), 5, "f"));
        assert_eq!(violation(refused), Some(BindingViolation::MissingScope));
    }

    #[test]
    fn test_unit_builder_references_one_over_limit_refused() {
        let limits = BindingLimits::builder().unit_references_max(1).build();
        let mut builder = UnitBindingFacts::builder(limits.unwrap_or_default());
        let root = builder
            .scope(ScopeKind::Module, range(0, 100), None)
            .expect("root scope accepted");
        builder
            .reference(reference(root, 5, "a"))
            .expect("first reference accepted");
        let refused = builder.reference(reference(root, 6, "b"));
        let expected = BindingViolation::UnitLimit(ExhaustedLimit::UnitReferences);
        assert_eq!(violation(refused), Some(expected));
    }

    #[test]
    fn test_unit_builder_import_unknown_scope_refused() {
        let mut builder = builder();
        let refused = builder.import(import(UnitScopeIndex(0), "f", "m::f"));
        assert_eq!(violation(refused), Some(BindingViolation::MissingScope));
    }

    #[test]
    fn test_unit_builder_links_share_one_bound_one_over_refused() {
        let limits = BindingLimits::builder().unit_links_max(2).build();
        let mut builder = UnitBindingFacts::builder(limits.unwrap_or_default());
        let root = builder
            .scope(ScopeKind::Module, range(0, 100), None)
            .expect("root scope accepted");
        let member = builder
            .scope(ScopeKind::Member, range(10, 20), Some(root))
            .expect("member scope accepted");
        builder
            .import(import(root, "f", "m::f"))
            .expect("import accepted");
        builder
            .member_link(UnitMemberLink::new(
                root,
                PathAnchor::Lexical,
                path("T"),
                member,
            ))
            .expect("member link accepted");
        let declaration = builder
            .definition(definition(root, "x", 0))
            .expect("definition accepted");
        let refused = builder.module_declaration(UnitModuleDeclaration::new(
            declaration,
            vec!["src/x.rs".to_owned()],
        ));
        let expected = BindingViolation::UnitLimit(ExhaustedLimit::UnitLinks);
        assert_eq!(violation(refused), Some(expected));
    }

    #[test]
    fn test_unit_builder_member_link_non_member_scope_refused() {
        let mut builder = builder();
        let root = builder
            .scope(ScopeKind::Module, range(0, 100), None)
            .expect("root scope accepted");
        let block = builder
            .scope(ScopeKind::Block, range(10, 20), Some(root))
            .expect("block scope accepted");
        let link = UnitMemberLink::new(root, PathAnchor::Lexical, path("T"), block);
        assert_eq!(
            violation(builder.member_link(link)),
            Some(BindingViolation::MemberScopeKind)
        );
        let missing = UnitMemberLink::new(root, PathAnchor::Lexical, path("T"), UnitScopeIndex(7));
        assert_eq!(
            violation(builder.member_link(missing)),
            Some(BindingViolation::MissingScope)
        );
    }

    #[test]
    fn test_unit_builder_module_declaration_unknown_definition_refused() {
        let mut builder = builder();
        let declaration =
            UnitModuleDeclaration::new(UnitDefinitionIndex(0), vec!["src/x.rs".to_owned()]);
        assert_eq!(
            violation(builder.module_declaration(declaration)),
            Some(BindingViolation::MissingDefinition)
        );
    }

    #[test]
    fn test_unit_builder_module_declaration_empty_candidates_refused() {
        let mut builder = builder();
        let root = builder
            .scope(ScopeKind::Module, range(0, 100), None)
            .expect("root scope accepted");
        let declaration = builder
            .definition(definition(root, "x", 0))
            .expect("definition accepted");
        let refused =
            builder.module_declaration(UnitModuleDeclaration::new(declaration, Vec::new()));
        assert_eq!(violation(refused), Some(BindingViolation::InvalidPath));
    }

    #[test]
    fn test_assemble_maps_unit_facts_into_graph_and_resolves() {
        let mut builder = builder();
        let root = builder
            .scope(ScopeKind::Module, range(0, 100), None)
            .expect("root scope accepted");
        builder
            .definition(definition(root, "f", 0))
            .expect("definition accepted");
        builder
            .reference(reference(root, 50, "f"))
            .expect("reference accepted");
        builder
            .import(import(root, "g", "m::g"))
            .expect("import accepted");
        let facts = builder.build();
        assert_eq!(facts.scopes().len(), 1);
        assert_eq!(facts.imports().len(), 1);
        assert_eq!(facts.member_links().len(), 0);
        assert_eq!(facts.module_declarations().len(), 0);
        let limits = BindingLimits::default();
        let units = [(source("src/lib.rs"), origin(), &facts)];
        let graph = assemble(&units, &limits).expect("facts assemble");
        assert_eq!(graph.scopes().len(), 1);
        assert_eq!(graph.definitions().len(), 1);
        assert_eq!(graph.references().len(), 1);
        assert_eq!(graph.links().len(), 1);
        let linked = LinkedGraph::link(&graph, &limits).expect("graph links");
        let set = resolve_all(&linked, &limits, &NeverCancelled).expect("resolution completes");
        let targets = set.resolutions()[0].targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(graph.definition(targets[0]).name().as_str(), "f");
    }

    /// Links the declaring unit against `others` and returns the declared unit's key.
    fn declared_unit_key(candidates: &[&str], others: &[&str]) -> Option<String> {
        let declaring = declaring_facts(candidates);
        let run = run_facts();
        let mut units = vec![(source("src/lib.rs"), origin(), &declaring)];
        for other in others {
            units.push((source(other), origin(), &run));
        }
        let limits = BindingLimits::default();
        let graph = assemble(&units, &limits).expect("facts assemble");
        let linked = LinkedGraph::link(&graph, &limits).expect("graph links");
        let target = graph.link_ids().find_map(|id| {
            matches!(graph.link(id).kind(), LinkKind::Module { .. })
                .then(|| linked.module_target(id))
                .flatten()
        });
        target.map(|scope| {
            let unit = graph.scope(scope).unit();
            graph.unit(unit).source().key().as_str().to_owned()
        })
    }

    #[test]
    fn test_assemble_module_declaration_first_present_candidate_wins() {
        let key = declared_unit_key(&["src/x.rs", "src/x/mod.rs"], &["src/x.rs", "src/x/mod.rs"]);
        assert_eq!(key.as_deref(), Some("src/x.rs"));
    }

    #[test]
    fn test_assemble_module_declaration_second_candidate_when_first_absent() {
        let key = declared_unit_key(&["src/x.rs", "src/x/mod.rs"], &["src/x/mod.rs"]);
        assert_eq!(key.as_deref(), Some("src/x/mod.rs"));
    }

    #[test]
    fn test_assemble_module_declaration_absent_candidates_leave_module_unlinked() {
        let declaring = declaring_facts(&["src/x.rs", "src/x/mod.rs"]);
        let units = [(source("src/lib.rs"), origin(), &declaring)];
        let limits = BindingLimits::default();
        let graph = assemble(&units, &limits).expect("facts assemble");
        let linked = LinkedGraph::link(&graph, &limits).expect("graph links");
        assert_eq!(linked.unlinked_modules().len(), 1);
        let set = resolve_all(&linked, &limits, &NeverCancelled).expect("resolution completes");
        assert_eq!(set.resolutions()[0].targets(), &[]);
    }

    #[test]
    fn test_assemble_module_declaration_invalid_candidates_emit_no_link() {
        let declaring = declaring_facts(&["../outside.rs"]);
        let units = [(source("src/lib.rs"), origin(), &declaring)];
        let limits = BindingLimits::default();
        let graph = assemble(&units, &limits).expect("facts assemble");
        assert_eq!(graph.links().len(), 0);
        let linked = LinkedGraph::link(&graph, &limits).expect("graph links");
        assert_eq!(linked.unlinked_modules(), &[]);
    }

    #[test]
    fn test_unit_facts_with_module_declarations_replaces_candidates() {
        let facts = declaring_facts(&["src/x.rs"]);
        let declaration = facts.module_declarations()[0].definition();
        let replaced = UnitModuleDeclaration::new(declaration, vec!["src/a/x.rs".to_owned()]);
        let refined = facts.with_module_declarations(vec![replaced], &BindingLimits::default());
        let refined = refined.expect("replacement accepted");
        assert_eq!(refined.module_declarations().len(), 1);
        assert_eq!(
            refined.module_declarations()[0].candidates(),
            ["src/a/x.rs".to_owned()]
        );
        assert_eq!(refined.definitions().len(), facts.definitions().len());
        let emptied = facts.with_module_declarations(Vec::new(), &BindingLimits::default());
        let emptied = emptied.expect("dropping every declaration accepted");
        assert_eq!(emptied.module_declarations(), &[]);
        let named = facts
            .definition(declaration)
            .map(|definition| definition.name().as_str());
        assert_eq!(named, Some("x"));
        assert!(facts.scope(facts.definitions()[0].scope()).is_some());
        assert_eq!(facts.definition(UnitDefinitionIndex(9)), None);
        assert_eq!(facts.scope(UnitScopeIndex(9)), None);
    }

    #[test]
    fn test_unit_facts_with_module_declarations_unknown_definition_refused() {
        let facts = declaring_facts(&["src/x.rs"]);
        let unknown =
            UnitModuleDeclaration::new(UnitDefinitionIndex(9), vec!["src/x.rs".to_owned()]);
        let refused = facts.with_module_declarations(vec![unknown], &BindingLimits::default());
        assert_eq!(
            violation(refused),
            Some(BindingViolation::MissingDefinition)
        );
    }

    #[test]
    fn test_unit_facts_with_module_declarations_empty_candidates_refused() {
        let facts = declaring_facts(&["src/x.rs"]);
        let declaration = facts.module_declarations()[0].definition();
        let empty = UnitModuleDeclaration::new(declaration, Vec::new());
        let refused = facts.with_module_declarations(vec![empty], &BindingLimits::default());
        assert_eq!(violation(refused), Some(BindingViolation::InvalidPath));
    }

    #[test]
    fn test_unit_facts_with_module_declarations_one_over_link_bound_refused() {
        let facts = declaring_facts(&["src/x.rs"]);
        let declaration = facts.module_declarations()[0].definition();
        let limits = BindingLimits::builder().unit_links_max(1).build();
        let limits = limits.unwrap_or_default();
        let at_bound = facts.with_module_declarations(
            vec![UnitModuleDeclaration::new(
                declaration,
                vec!["src/x.rs".to_owned()],
            )],
            &limits,
        );
        assert!(at_bound.is_ok(), "one declaration meets the bound of one");
        let over = vec![
            UnitModuleDeclaration::new(declaration, vec!["src/x.rs".to_owned()]),
            UnitModuleDeclaration::new(declaration, vec!["src/x/mod.rs".to_owned()]),
        ];
        let refused = facts.with_module_declarations(over, &limits);
        let expected = BindingViolation::UnitLimit(ExhaustedLimit::UnitLinks);
        assert_eq!(violation(refused), Some(expected));
    }

    #[test]
    fn test_assemble_empty_units_build_empty_graph() {
        let graph = assemble(&[], &BindingLimits::default()).expect("empty set assembles");
        assert_eq!(graph.units().len(), 0);
        assert_eq!(graph.scopes().len(), 0);
    }

    #[test]
    fn test_unit_rows_expose_their_facts() {
        let member_link = UnitMemberLink::new(
            UnitScopeIndex(0),
            PathAnchor::Crate,
            path("m::T"),
            UnitScopeIndex(1),
        );
        assert_eq!(member_link.scope(), UnitScopeIndex(0));
        assert_eq!(member_link.owner_anchor(), PathAnchor::Crate);
        assert_eq!(member_link.owner(), &path("m::T"));
        assert_eq!(member_link.member(), UnitScopeIndex(1));
        let declaration =
            UnitModuleDeclaration::new(UnitDefinitionIndex(2), vec!["src/x.rs".to_owned()]);
        assert_eq!(declaration.definition(), UnitDefinitionIndex(2));
        assert_eq!(declaration.candidates(), ["src/x.rs".to_owned()]);
        let entry = import(UnitScopeIndex(0), "f", "m::f");
        assert_eq!(entry.name(), Some(&name("f")));
        assert_eq!(entry.anchor(), PathAnchor::SelfModule);
        assert_eq!(entry.path(), &path("m::f"));
        assert_eq!(entry.visibility(), VisibilitySpelling::Private);
        assert_eq!(entry.rank(), IMPORT_EXPLICIT_RANK);
        let occurrence = reference(UnitScopeIndex(0), 5, "f");
        assert_eq!(occurrence.scope(), UnitScopeIndex(0));
        assert_eq!(occurrence.range(), range(5, 6));
        assert_eq!(occurrence.anchor(), PathAnchor::Lexical);
        assert_eq!(occurrence.path(), &path("f"));
        assert_eq!(occurrence.role(), ReferenceRole::Read);
        let declared = definition(UnitScopeIndex(0), "f", 0);
        assert_eq!(declared.kind(), &kind());
        assert_eq!(declared.facets(), &[]);
        assert_eq!(declared.order(), DefinitionOrder::Item);
        assert_eq!(declared.visibility(), VisibilitySpelling::Public);
        assert!(!declared.is_item());
        assert_eq!(declared.declares(), None);
    }
}
